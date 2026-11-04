//! rumdl: its JSON diagnostics, and the project config supdoc finds for it.
//!
//! rumdl finds its config from the directory it runs in, never from the block's virtual path,
//! and supdoc runs it from an empty directory of its own. So supdoc walks up from the doc inside
//! the root, passes the config it finds by `--config`, and refuses one that could make rumdl run
//! a command or read a file the result cache doesn't key on: a `code-block-tools` section, a
//! `command`, or an `extends`. With no config found, rumdl gets `--no-config`, so a config in
//! the home directory never applies.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use super::format::ParseError;
use super::lint::{self, Diagnostic};
use super::runner::{Failure, Ran};

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
pub(crate) struct Refusal {
    /// The project's rumdl config, relative to the root.
    pub(crate) file: String,
    /// What is wrong with it.
    pub(crate) why: &'static str,
}

/// The flags that hand rumdl `doc`'s project config, found walking up from `doc`'s directory to
/// `root`, or `--no-config` when there is none.
///
/// Effect: reads file metadata and the config, and resolves symlinks.
///
/// # Errors
///
/// [`Refusal`] when the config found is a link out of the root, isn't a readable TOML file of at
/// most 256 KiB, extends anything, or declares commands.
pub(crate) fn project_args(root: &Path, doc: &Path) -> Result<Vec<OsString>, Refusal> {
    for candidate in candidates(root, doc) {
        if let Some(real) = checked(root, &candidate)? {
            let mut flag = OsString::from("--config=");
            flag.push(real);
            return Ok(vec![flag]);
        }
    }
    Ok(vec![OsString::from("--no-config")])
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

/// `found`'s real path when it is a safe rumdl config of the project at `root`; `None` for a
/// `pyproject.toml` with no `[tool.rumdl]` table, which rumdl doesn't count as a config.
///
/// # Errors
///
/// [`Refusal`] naming what is wrong with it.
fn checked(root: &Path, found: &Path) -> Result<Option<PathBuf>, Refusal> {
    let file = found
        .strip_prefix(root)
        .unwrap_or(found)
        .display()
        .to_string();
    let refuse = |why| Refusal {
        file: file.clone(),
        why,
    };
    let real_root = root
        .canonicalize()
        .map_err(|_| refuse("couldn't be resolved"))?;
    let real = found
        .canonicalize()
        .map_err(|_| refuse("couldn't be resolved"))?;
    if !real.starts_with(&real_root) {
        return Err(refuse("is a link that resolves outside the project"));
    }
    let meta = fs::metadata(&real).map_err(|_| refuse("couldn't be read"))?;
    if !meta.is_file() || meta.len() > MAX_CONFIG {
        return Err(refuse("isn't a regular file of at most 256 KiB"));
    }
    let text = fs::read_to_string(&real).map_err(|_| refuse("couldn't be read as UTF-8"))?;
    let Ok(table) = text.parse::<toml::Table>() else {
        return Err(refuse("doesn't parse as TOML"));
    };
    let is_pyproject = found
        .file_name()
        .is_some_and(|name| name == "pyproject.toml");
    let rumdl = if is_pyproject {
        match table
            .get("tool")
            .and_then(toml::Value::as_table)
            .and_then(|tool| tool.get("rumdl"))
        {
            Some(toml::Value::Table(rumdl)) => rumdl,
            _ => return Ok(None),
        }
    } else {
        &table
    };
    match unsafe_part(rumdl) {
        Some(why) => Err(refuse(why)),
        None => Ok(Some(real)),
    }
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
    use super::{Refusal, diagnostics, project_args, unsafe_part};
    use crate::code_blocks::lint::Diagnostic;
    use std::ffi::OsString;
    use std::fs;

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
        assert_eq!(
            refusal_of("[global]\nline-length = 20\n[MD013]\nenabled = false\n"),
            None
        );
    }

    #[test]
    fn the_nearest_config_inside_the_root_is_passed_and_an_unsafe_one_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::create_dir_all(root.path().join("docs/deep")).expect("the fixture dirs");
        fs::write(root.path().join(".rumdl.toml"), "extends = \"x.toml\"\n").expect("root config");
        fs::write(root.path().join("docs/rumdl.toml"), "[global]\n").expect("docs config");
        let near = root.path().join("docs/deep/a.md");
        let far = root.path().join("a.md");

        let passed = project_args(root.path(), &near).expect("the docs config is safe");
        let refused = project_args(root.path(), &far);

        let real = root
            .path()
            .canonicalize()
            .expect("the root resolves")
            .join("docs/rumdl.toml");
        let mut expected = OsString::from("--config=");
        expected.push(real);
        assert_eq!(passed, [expected]);
        assert!(
            matches!(refused, Err(Refusal { ref file, .. }) if file == ".rumdl.toml"),
            "got {refused:?}"
        );
    }

    #[test]
    fn a_pyproject_counts_only_with_a_tool_rumdl_table() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let doc = root.path().join("a.md");
        fs::write(root.path().join("pyproject.toml"), "[tool.ruff]\nx = 1\n").expect("pyproject");
        let without = project_args(root.path(), &doc);
        fs::write(
            root.path().join("pyproject.toml"),
            "[tool.rumdl.global]\nline-length = 20\n",
        )
        .expect("pyproject");
        let with = project_args(root.path(), &doc);

        assert_eq!(without, Ok(vec![OsString::from("--no-config")]));
        assert!(
            matches!(with.as_deref(), Ok([flag]) if flag.to_string_lossy().starts_with("--config=")),
            "got {with:?}"
        );
    }

    #[test]
    fn a_config_that_is_a_link_out_of_the_root_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let outside = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::write(outside.path().join("r.toml"), "[global]\n").expect("outside config");
        std::os::unix::fs::symlink(
            outside.path().join("r.toml"),
            root.path().join(".rumdl.toml"),
        )
        .expect("the link");

        let refused = project_args(root.path(), &root.path().join("a.md"));

        assert!(matches!(refused, Err(Refusal { .. })), "got {refused:?}");
    }
}
