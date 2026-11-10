//! mbake: the config supdoc gives it, its rule errors, its version, and the blocks it is never
//! given.
//!
//! supdoc runs mbake only as `mbake format --stdin --config <file>`: the block on stdin, the
//! formatted Makefile on stdout. `format --stdin` is in-process Python code. supdoc never runs
//! `mbake --version` (it asks PyPI for a newer release over the network), `mbake validate` or
//! `format --validate` (both run `make`, which expands `$(shell ...)`).
//!
//! Without `--config`, mbake takes the nearest `.bake.toml` in the directory it runs in or a
//! parent, else `~/.bake.toml`, else `$XDG_CONFIG_HOME/bake.toml` (`mbake/config.py`
//! `Config.load_or_default`), so supdoc always passes `--config` with a private file of its own
//! and mbake searches nowhere. supdoc finds the project's `.bake.toml` itself, walking up from
//! the doc to the root, and writes again only the `[formatter]` keys mbake reads
//! (`config.py` `valid_formatter_keys`), each checked for its type. Every other key is dropped:
//! `debug` and `verbose` (more output), `gnu_error_format` and `wrap_error_messages` (the shape of
//! the rule errors supdoc reads), and `syntax_check_timeout` (only `--validate` reads it).
//!
//! mbake judges no other in-content setting: its one directive is `# bake-format off`, matched by
//! `^\s*#\s*bake-format\s+off\b` in any case (`mbake/utils/format_disable.py:22-24`), which
//! leaves the lines after it as written. A block holding it is never given (see [`not_given`]),
//! nor is one holding a carriage return: mbake strips it on Windows and may rewrite it elsewhere.
//!
//! mbake's version can't be read by running it, so supdoc reads the `METADATA` of the
//! `mbake-*.dist-info` its interpreter imports, a plain file (see [`version_of`]).

use std::ffi::OsString;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use super::format::ParseError;
use super::runner;
use super::tools::ToolEnv;
use crate::config::confine::{Boundary, ReadEscape};
use crate::hash::Hash;
use crate::path::{self, Component, fixed};

/// The most bytes of a config supdoc reads.
const MAX_CONFIG: u64 = 256 * 1024;

/// The `[formatter]` switches supdoc keeps from the project's config, each `true` or `false`.
const SWITCHES: [&str; 14] = [
    "space_around_assignment",
    "space_before_colon",
    "space_after_colon",
    "normalize_line_continuations",
    "auto_insert_phony_declarations",
    "group_phony_declarations",
    "phony_at_top",
    "remove_trailing_whitespace",
    "ensure_final_newline",
    "normalize_empty_lines",
    "fix_missing_recipe_tabs",
    "indent_nested_conditionals",
    "align_variable_assignments",
    "align_across_comments",
];

/// The `[formatter]` counts supdoc keeps from the project's config, each a whole number from 0
/// to [`MAX_COUNT`].
const COUNTS: [&str; 3] = [
    "max_line_length",
    "max_consecutive_empty_lines",
    "tab_width",
];

/// The largest count supdoc writes again.
const MAX_COUNT: i64 = 10_000;

/// The comment that makes mbake leave the lines after it as written.
const SWITCH: &str = "bake-format";

/// Why a block's format step didn't run when it holds `bake-format`.
pub(crate) const SWITCHED_OFF: &str = "the block holds `bake-format`, so supdoc can't confirm \
                                       mbake formats all of it; drop the comment, or mark the \
                                       block `format(off: <reason>)`";

/// Why a block's format step didn't run when it holds a carriage return.
pub(crate) const CARRIAGE_RETURN: &str = "the block holds a carriage return, which mbake would \
                                          strip or rewrite";

/// Why `input`, a Makefile block as its formatter reads it, isn't given to mbake: it holds a
/// carriage return ([`CARRIAGE_RETURN`]), or `bake-format` in any case ([`SWITCHED_OFF`]). `None`
/// when mbake may run.
///
/// The `bake-format` scan reads text, not comments, so the words inside a recipe count too, as
/// does a near miss such as `# bake-format offx` or `#bake-format on`: a near miss switches the
/// step off, never counts as a pass. An invalid UTF-8 block can't reach here: supdoc reads every
/// block as UTF-8 text.
#[must_use]
pub(crate) fn not_given(input: &str) -> Option<&'static str> {
    if input.contains('\r') {
        return Some(CARRIAGE_RETURN);
    }
    input
        .to_ascii_lowercase()
        .contains(SWITCH)
        .then_some(SWITCHED_OFF)
}

/// The block line mbake's first rule error in `stderr` names: `<n>: Error: ...`, or
/// `Makefile:<n>: Error: ...`, 1-based (mbake exits 2 for it). `None` for any other stderr.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    stderr.lines().find_map(|line| {
        let line = line.trim_start();
        let rest = line.strip_prefix("Makefile:").unwrap_or(line);
        let (number, message) = rest.split_once(':')?;
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if !message.trim_start().starts_with("Error:") {
            return None;
        }
        Some(ParseError {
            line: number.parse::<usize>().ok()?.checked_sub(1),
        })
    })
}

/// The mbake versions, as `<major>.<minor>`, supdoc's checks are verified against: supdoc writes
/// mbake's config and reads its rule errors, and another version may read or write either
/// differently. A minor is added only after the Makefile e2e suite passes against it.
pub(crate) const VERIFIED: [&str; 1] = ["1.5"];

/// How an mbake whose version can't be read is named.
const UNREADABLE: &str = "(whose version supdoc couldn't read)";

/// How an mbake whose interpreter also imports an old `bake` module is named: mbake then prints
/// an upgrade notice on stdout ahead of the formatted Makefile.
const BESIDE_BAKE: &str = "(beside an old `bake` module, which makes it print a notice into its \
                           output)";

/// The most bytes of a `METADATA` file or a script's first line supdoc reads.
const MAX_METADATA: u64 = 64 * 1024;

/// The version of the mbake `program` runs, read from its package's `METADATA`, never by running
/// it, when it is one supdoc is verified against (see [`VERIFIED`]). `program` is the real path
/// of the `mbake` on `PATH`.
///
/// The package is looked for under two prefixes, in order: that of the interpreter the script's
/// `#!` line names, when it is an absolute path to a `python*` file (a venv, pipx, uv or Homebrew
/// script), then the directory above the script's own (`<prefix>/bin/mbake`, or
/// `<prefix>\Scripts\mbake.exe` on Windows). Under a prefix the package directories are
/// `lib/python*/site-packages`, `lib/python*/dist-packages` and `Lib/site-packages`. The first
/// one holding an `mbake-*.dist-info` decides: it must hold exactly one, and no `bake` module.
///
/// Effect: reads the script's first bytes, lists package directories and reads one `METADATA`.
///
/// # Errors
///
/// How to name the version to the user: the version `METADATA` gives, or, when there is none
/// supdoc can read or an old `bake` module sits beside it, that.
pub(crate) fn version_of(program: &Path) -> Result<String, String> {
    let prefixes = interpreter_prefix(program).into_iter().chain(
        program
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf),
    );
    for prefix in prefixes {
        for packages in package_dirs(&prefix) {
            let infos = dist_infos(&packages);
            match infos.as_slice() {
                [] => {}
                [info] => {
                    if holds_bake(&packages) {
                        return Err(BESIDE_BAKE.to_owned());
                    }
                    let text = read_capped(&path::beneath(info, &fixed::METADATA))
                        .ok_or_else(|| UNREADABLE.to_owned())?;
                    return verified(&text);
                }
                _ => return Err(UNREADABLE.to_owned()),
            }
        }
    }
    Err(UNREADABLE.to_owned())
}

/// The version `metadata`, an `mbake` package's `METADATA`, gives in its `Version:` header when it
/// is one supdoc is verified against (see [`VERIFIED`]).
///
/// # Errors
///
/// How to name the version to the user: the version given, or, when the headers name another
/// package or no `<major>.<minor>.<patch>` version, that.
pub(crate) fn verified(metadata: &str) -> Result<String, String> {
    let headers: Vec<&str> = metadata
        .lines()
        .take_while(|line| !line.is_empty())
        .collect();
    let header = |name: &str| {
        headers
            .iter()
            .find_map(|line| line.strip_prefix(name).map(str::trim))
    };
    if !header("Name:").is_some_and(|name| name.eq_ignore_ascii_case("mbake")) {
        return Err(UNREADABLE.to_owned());
    }
    let version = header("Version:")
        .filter(|version| {
            let parts: Vec<&str> = version.split('.').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        })
        .ok_or_else(|| UNREADABLE.to_owned())?;
    let minor = version.rsplit_once('.').map_or(version, |(minor, _)| minor);
    if VERIFIED.contains(&minor) {
        Ok(version.to_owned())
    } else {
        Err(version.to_owned())
    }
}

/// The prefix of the interpreter `program`'s `#!` line names (`<prefix>/bin/python3`), as
/// written, never resolved: a venv's `python` links to the interpreter it was made from, whose
/// prefix holds other packages. `None` when the line names no absolute `python*` file.
///
/// Effect: reads the script's first bytes.
fn interpreter_prefix(program: &Path) -> Option<PathBuf> {
    let text = read_capped(program)?;
    let line = text.lines().next()?.strip_prefix("#!")?;
    let interpreter = Path::new(line.split_whitespace().next()?);
    let python = interpreter.file_name()?.to_str()?.starts_with("python");
    (interpreter.is_absolute() && python)
        .then(|| interpreter.parent()?.parent().map(Path::to_path_buf))
        .flatten()
}

/// The package directories under `prefix` that exist, in the order [`version_of`] names them.
///
/// Effect: lists `<prefix>/lib` and reads metadata.
fn package_dirs(prefix: &Path) -> Vec<PathBuf> {
    let lib = path::beneath(prefix, &fixed::LIB);
    let mut versions: Vec<PathBuf> = fs::read_dir(&lib)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            name.starts_with("python")
                .then(|| Component::new(name))
                .flatten()
                .map(|name| path::beneath(&lib, &name))
        })
        .collect();
    versions.sort();
    versions
        .iter()
        .flat_map(|version| {
            [fixed::SITE_PACKAGES, fixed::DIST_PACKAGES].map(|name| path::beneath(version, &name))
        })
        .chain([path::beneath(prefix, &fixed::WINDOWS_SITE_PACKAGES)])
        .filter(|dir| dir.is_dir())
        .collect()
}

/// The `mbake-*.dist-info` directories in `packages`.
///
/// Effect: lists `packages`.
fn dist_infos(packages: &Path) -> Vec<PathBuf> {
    fs::read_dir(packages)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let lower = name.to_ascii_lowercase();
            (lower.starts_with("mbake-") && lower.ends_with(".dist-info"))
                .then(|| Component::new(name))
                .flatten()
                .map(|name| path::beneath(packages, &name))
        })
        .collect()
}

/// Whether `packages` holds a `bake` module or package, which `mbake/cli.py:28-38` detects and
/// then prints an upgrade notice on stdout for.
///
/// Effect: lists `packages`.
fn holds_bake(packages: &Path) -> bool {
    fs::read_dir(packages)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let name = entry.file_name();
            name.to_str().is_some_and(|name| {
                matches!(name.to_ascii_lowercase().as_str(), "bake" | "bake.py")
            })
        })
}

/// The first [`MAX_METADATA`] bytes of `file`, read as UTF-8 with any invalid byte replaced.
/// `None` when it can't be read.
///
/// Effect: reads the file.
fn read_capped(file: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    let _read = fs::File::open(file)
        .ok()?
        .take(MAX_METADATA)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Why a block isn't given to mbake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The project's mbake config, at `file` (relative to the root), can't be given to mbake.
    Config {
        /// The config's path relative to the root.
        file: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// supdoc has no private directory to keep mbake's config in.
    NoPrivateDir,
}

/// What mbake is given for the blocks of one doc directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Project {
    /// `--config` and the private file, always: mbake never searches for a config.
    pub(crate) flags: Vec<OsString>,
}

/// The [`Project`] mbake is given for `doc`: the project's `.bake.toml`, found walking up from
/// `doc`'s directory to `root`, its kept keys (see [`kept`]) written into a private file named by
/// the hash of what is written; with no config, a private file holding only `[formatter]`, which
/// mbake reads as its defaults.
///
/// Effect: reads file metadata and the config, resolves symlinks, and may create supdoc's
/// private directory and the config file in it.
///
/// # Errors
///
/// [`Refusal::Config`] when the config found is a link out of the root, isn't a readable UTF-8
/// file of at most 256 KiB, or is one supdoc doesn't write again (see [`kept`]);
/// [`Refusal::NoPrivateDir`] when there is nowhere private to put it.
pub(crate) fn project_args(env: &ToolEnv, root: &Path, doc: &Path) -> Result<Project, Refusal> {
    let (file, text) = match candidates(root, doc).next() {
        Some(found) => {
            let file = relative(root, &found);
            let refuse = |why| Refusal::Config {
                file: file.clone(),
                why,
            };
            let text = read_text(root, &found).map_err(refuse)?;
            (Some(file.clone()), kept(&text).map_err(refuse)?)
        }
        None => (None, kept("").unwrap_or_default()),
    };
    let private = private_config(env, root, &text).ok_or(match file {
        Some(file) => Refusal::Config {
            file,
            why: "couldn't be written again into supdoc's private directory",
        },
        None => Refusal::NoPrivateDir,
    })?;
    Ok(Project {
        flags: vec![OsString::from("--config"), private.into_os_string()],
    })
}

/// `path` relative to `root`, as shown to the user.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// The `.bake.toml` files in `doc`'s directory and its ancestors, nearest first, never above
/// `root`.
fn candidates(root: &Path, doc: &Path) -> impl Iterator<Item = PathBuf> {
    let dirs: Vec<PathBuf> = doc
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take_while(|dir| dir.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    dirs.into_iter()
        .map(|dir| path::beneath(&dir, &fixed::DOT_BAKE_TOML))
        .filter(|candidate| fs::symlink_metadata(candidate).is_ok())
}

/// The text of the config at `found`, which must resolve inside `root`.
///
/// # Errors
///
/// What is wrong with the file.
fn read_text(root: &Path, found: &Path) -> Result<String, &'static str> {
    let real = Boundary::resolve(root)
        .real_inside(found)
        .map_err(|escape| match escape {
            ReadEscape::Outside { .. } => "is a link that resolves outside the project",
            ReadEscape::BoundaryUnresolved | ReadEscape::Unresolved => "couldn't be resolved",
        })?;
    let meta = fs::metadata(&real).map_err(|_| "couldn't be read")?;
    if !meta.is_file() || meta.len() > MAX_CONFIG {
        return Err("isn't a regular file of at most 256 KiB");
    }
    fs::read_to_string(&real).map_err(|_| "couldn't be read as UTF-8")
}

/// What supdoc writes again of the config `text`: a `[formatter]` table holding each of its
/// [`SWITCHES`] and [`COUNTS`], in that order, and nothing else. A `[formatter]` key mbake doesn't
/// read and every other key are dropped.
///
/// # Errors
///
/// Why supdoc doesn't write the config again: it isn't TOML, its `formatter` isn't a table, or a
/// kept key has a value of the wrong kind, which mbake would fail on or read differently.
fn kept(text: &str) -> Result<String, &'static str> {
    let table: toml::Table = text.parse().map_err(|_| "isn't valid TOML")?;
    let formatter = match table.get("formatter") {
        None => toml::Table::new(),
        Some(toml::Value::Table(formatter)) => formatter.clone(),
        Some(_) => return Err("sets `formatter` to a value that isn't a table"),
    };
    let mut copy = String::from("[formatter]\n");
    for key in SWITCHES {
        if let Some(value) = formatter.get(key) {
            let on = value
                .as_bool()
                .ok_or("sets a `[formatter]` switch to a value that isn't `true` or `false`")?;
            for part in [key, " = ", if on { "true" } else { "false" }, "\n"] {
                copy.push_str(part);
            }
        }
    }
    for key in COUNTS {
        if let Some(value) = formatter.get(key) {
            let count = value
                .as_integer()
                .filter(|count| (0..=MAX_COUNT).contains(count))
                .ok_or("sets a `[formatter]` count to a value that isn't a whole number from 0 to 10000")?;
            for part in [key, " = ", &count.to_string(), "\n"] {
                copy.push_str(part);
            }
        }
    }
    Ok(copy)
}

/// Writes `text` into `bake.toml` in a private directory named by the hash of `text`, under
/// supdoc's `mbake-config` directory, and returns the file's path. A file already holding the
/// same content is kept; anything else at the name is replaced by writing a temporary file beside
/// it and renaming it into place, so concurrent runs never see half a file.
///
/// Effect: creates the directories mode 0700 and the file mode 0600, and may replace a stale one.
///
/// `None` when there is no private directory or the file can't be written.
fn private_config(env: &ToolEnv, root: &Path, text: &str) -> Option<PathBuf> {
    let neutral = runner::neutral_dir(env, root);
    if neutral == Path::new("/") {
        return None;
    }
    let cache = runner::exact(neutral.parent()?)?;
    let parent = cache.beneath(&fixed::MBAKE_CONFIG_DIR);
    let digest = Component::new(&Hash::digest(text.as_bytes()).to_string())?;
    let dir = parent.beneath(&digest);
    let file = dir.beneath(&fixed::BAKE_TOML);
    let parent_boundary = runner::create_private(&Boundary::resolve(&cache), &parent)?;
    let own = runner::create_private(&parent_boundary, &dir)?;
    runner::write_private(&own, &file, text)?;
    Some(file.as_path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_carriage_return_or_bake_format_in_any_spelling_withholds_the_block() {
        let cases = [
            ("all:\n\techo hi\n", None),
            ("A = 1\n", None),
            ("", None),
            ("all:\r\n\techo hi\r\n", Some(CARRIAGE_RETURN)),
            ("a\rb\n", Some(CARRIAGE_RETURN)),
            ("# bake-format off\nA=1\n", Some(SWITCHED_OFF)),
            ("#BAKE-FORMAT   Off\nA=1\n", Some(SWITCHED_OFF)),
            ("# bake-format on\n", Some(SWITCHED_OFF)),
            ("# bake-format offx\nA=1\n", Some(SWITCHED_OFF)),
            ("all:\n\t@echo hi # bake-format off\n", Some(SWITCHED_OFF)),
            ("# bakeformat off\n", None),
        ];
        for (input, expected) in cases {
            assert_eq!(not_given(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn a_rule_error_is_read_at_its_one_based_line() {
        let read = |stderr: &str| parse_error(stderr).map(|error| error.line);

        assert_eq!(
            read("3: Error: Duplicate target 'all' defined at lines 1 and 3.\n"),
            Some(Some(2))
        );
        assert_eq!(read("Makefile:1: Error: x\n"), Some(Some(0)));
        assert_eq!(read("2: Warning: x\n"), None);
        assert_eq!(read("Error: no line\n"), None);
        assert_eq!(
            read("Fatal error: Failed to parse configuration file\n"),
            None
        );
        assert_eq!(read(""), None);
    }

    #[test]
    fn only_a_verified_mbake_version_in_its_metadata_is_verified() {
        let metadata = |name: &str, version: &str| {
            format!("Metadata-Version: 2.4\nName: {name}\nVersion: {version}\n\nVersion: 1.5.0\n")
        };

        assert_eq!(
            verified(&metadata("mbake", "1.5.0")),
            Ok("1.5.0".to_owned())
        );
        assert_eq!(
            verified(&metadata("mbake", "1.5.12")),
            Ok("1.5.12".to_owned())
        );
        assert_eq!(
            verified(&metadata("mbake", "1.4.9")),
            Err("1.4.9".to_owned())
        );
        assert_eq!(
            verified(&metadata("mbake", "1.6.0")),
            Err("1.6.0".to_owned())
        );
        assert_eq!(
            verified(&metadata("mbake", "1.5")),
            Err(UNREADABLE.to_owned())
        );
        assert_eq!(
            verified(&metadata("mbake", "1.5.0rc1")),
            Err(UNREADABLE.to_owned())
        );
        assert_eq!(
            verified(&metadata("bake", "1.5.0")),
            Err(UNREADABLE.to_owned())
        );
        assert_eq!(
            verified("Name: mbake\n\nVersion: 1.5.0\n"),
            Err(UNREADABLE.to_owned()),
            "a Version line in the body is not a header"
        );
    }

    /// `rel`, `/`-separated, beneath `base`.
    fn at(base: &Path, rel: &str) -> PathBuf {
        rel.split('/')
            .filter_map(Component::new)
            .fold(base.to_path_buf(), |dir, part| path::beneath(&dir, &part))
    }

    /// Writes `text` at `rel` beneath `base`, creating its parents.
    fn write(base: &Path, rel: &str, text: &str) {
        let file = at(base, rel);
        fs::create_dir_all(file.parent().expect("a written file has a parent"))
            .expect("the parents should be creatable");
        fs::write(&file, text).expect("the file should be writable");
    }

    /// [`write`]s `text` at `rel` beneath `base` and returns its path.
    fn put(base: &Path, rel: &str, text: &str) -> PathBuf {
        write(base, rel, text);
        at(base, rel)
    }

    /// An `mbake` `METADATA` for `version`.
    fn metadata(version: &str) -> String {
        format!("Metadata-Version: 2.4\nName: mbake\nVersion: {version}\n")
    }

    #[test]
    fn the_version_is_read_from_the_metadata_beside_the_scripts_own_prefix() {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let script = put(dir.path(), "venv/bin/mbake", "#!/bin/sh\n");
        write(
            dir.path(),
            "venv/lib/python3.12/site-packages/mbake-1.5.0.dist-info/METADATA",
            &metadata("1.5.0"),
        );

        assert_eq!(version_of(&script), Ok("1.5.0".to_owned()));
    }

    #[test]
    fn the_interpreter_the_shebang_names_is_read_before_the_scripts_own_prefix() {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let python = at(dir.path(), "libexec/bin/python3");
        let script = put(
            dir.path(),
            "bin/mbake",
            &format!("#!{}\nimport sys\n", python.display()),
        );
        write(
            dir.path(),
            "libexec/lib/python3.13/site-packages/mbake-1.4.0.dist-info/METADATA",
            &metadata("1.4.0"),
        );
        write(
            dir.path(),
            "lib/python3.13/site-packages/mbake-1.5.0.dist-info/METADATA",
            &metadata("1.5.0"),
        );

        assert_eq!(version_of(&script), Err("1.4.0".to_owned()));
    }

    #[test]
    fn a_dist_packages_or_windows_layout_is_read_too() {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let debian = put(dir.path(), "usr/local/bin/mbake", "#!/bin/sh\n");
        write(
            dir.path(),
            "usr/local/lib/python3.12/dist-packages/mbake-1.5.0.dist-info/METADATA",
            &metadata("1.5.0"),
        );
        let windows = put(dir.path(), "win/Scripts/mbake.exe", "MZ");
        write(
            dir.path(),
            "win/Lib/site-packages/mbake-1.5.1.dist-info/METADATA",
            &metadata("1.5.1"),
        );

        assert_eq!(version_of(&debian), Ok("1.5.0".to_owned()));
        assert_eq!(version_of(&windows), Ok("1.5.1".to_owned()));
    }

    #[test]
    fn two_installs_none_or_an_old_bake_module_leave_the_version_unread() {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let none = put(dir.path(), "none/bin/mbake", "#!/bin/sh\n");
        let two = put(dir.path(), "two/bin/mbake", "#!/bin/sh\n");
        for version in ["1.5.0", "1.4.0"] {
            write(
                dir.path(),
                &format!("two/lib/python3.12/site-packages/mbake-{version}.dist-info/METADATA"),
                &metadata(version),
            );
        }
        let bake = put(dir.path(), "bake/bin/mbake", "#!/bin/sh\n");
        write(
            dir.path(),
            "bake/lib/python3.12/site-packages/mbake-1.5.0.dist-info/METADATA",
            &metadata("1.5.0"),
        );
        write(
            dir.path(),
            "bake/lib/python3.12/site-packages/bake/__init__.py",
            "",
        );

        assert_eq!(version_of(&none), Err(UNREADABLE.to_owned()));
        assert_eq!(version_of(&two), Err(UNREADABLE.to_owned()));
        assert_eq!(version_of(&bake), Err(BESIDE_BAKE.to_owned()));
    }

    #[test]
    fn the_copy_keeps_only_typed_formatter_keys_mbake_reads() {
        let text = "debug = true\nverbose = true\ngnu_error_format = false\n\
                    syntax_check_timeout = 5\n[formatter]\ntab_width = 4\n\
                    space_around_assignment = false\nunknown_key = \"x\"\n\
                    max_line_length = 80\n[other]\nx = 1\n";

        assert_eq!(
            kept(text),
            Ok("[formatter]\nspace_around_assignment = false\nmax_line_length = 80\ntab_width = 4\n"
                .to_owned())
        );
        assert_eq!(kept(""), Ok("[formatter]\n".to_owned()));
        assert_eq!(kept("debug = true\n"), Ok("[formatter]\n".to_owned()));
    }

    #[test]
    fn a_config_supdoc_cant_write_again_is_refused() {
        let cases = [
            "[formatter\n",
            "formatter = 1\n",
            "[formatter]\nphony_at_top = \"yes\"\n",
            "[formatter]\nphony_at_top = 1\n",
            "[formatter]\ntab_width = \"4\"\n",
            "[formatter]\ntab_width = 4.0\n",
            "[formatter]\ntab_width = -1\n",
            "[formatter]\nmax_line_length = 10001\n",
        ];
        for text in cases {
            assert!(kept(text).is_err(), "config {text:?} should be refused");
        }
    }
}
