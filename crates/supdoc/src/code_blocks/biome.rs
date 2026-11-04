//! biome: the extension a tag selects, its syntax-error report, and the project config supdoc
//! finds for it.
//!
//! biome finds its config from the directory it runs in, never from the block's virtual path,
//! and supdoc runs it from an empty directory of its own. So supdoc walks up from the doc inside
//! the root, passes the config it finds by `--config-path`, and refuses one that could make
//! biome load code the checkout chose: an `extends` (a package from `node_modules`, or any other
//! file whose content the result cache doesn't key on) or a `plugins` list. With no config
//! found, biome gets a supdoc-owned empty one, so a config in the home directory never applies.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::format::ParseError;
use super::runner;
use super::tools::ToolEnv;

/// The config names biome looks for in a directory, in its own order.
const CONFIG_FILES: [&str; 4] = ["biome.json", "biome.jsonc", ".biome.json", ".biome.jsonc"];

/// The most bytes of a config supdoc reads.
const MAX_CONFIG: u64 = 64 * 1024;

/// The content of the config biome gets when the project has none.
const EMPTY_CONFIG: &str = "{}";

/// The file extension biome reads a block tagged `tag` by: the tag itself when it names a
/// module or JSX flavor (`mts`, `cts`, `tsx`, `mjs`, `cjs`, `jsx`), else `ts` or `js`.
#[must_use]
pub(crate) fn extension(tag: &str) -> &'static str {
    match tag {
        "mts" => "mts",
        "cts" => "cts",
        "tsx" => "tsx",
        "mjs" => "mjs",
        "cjs" => "cjs",
        "jsx" => "jsx",
        "js" | "javascript" => "js",
        _ => "ts",
    }
}

/// `biome format`'s syntax-error report in `stderr`: a header line `<path>:L:C parse ━━━` whose
/// path is a block's virtual path (it ends `.<index>.<extension>`). A header naming any other
/// file, such as a config, is not a parse error of the block.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    let header = stderr.lines().find(|line| line.contains(" parse ━"))?;
    let (place, _) = header.split_once(" parse ")?;
    let mut parts = place.rsplitn(3, ':');
    let _column = parts.next()?.parse::<usize>().ok()?;
    let line = parts.next()?.parse::<usize>().ok()?;
    let path = parts.next()?;
    let (rest, extension) = path.rsplit_once('.')?;
    let (_, index) = rest.rsplit_once('.')?;
    let virtual_path = index.parse::<usize>().is_ok()
        && ["ts", "mts", "cts", "tsx", "js", "mjs", "cjs", "jsx"].contains(&extension);
    virtual_path.then_some(ParseError {
        line: line.checked_sub(1),
    })
}

/// Why a block isn't given to biome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The project's biome config, at `file` (relative to the root), can't be given to biome.
    Config {
        /// The config's path relative to the root.
        file: String,
        /// What is wrong with it.
        why: String,
        /// What to do about it.
        help: String,
    },
    /// supdoc has no private directory to keep the config it gives biome in.
    NoPrivateDir,
}

/// What biome is given for a block: the flags, and where its doc stands in for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Given {
    /// `--config-path=<the private file>`.
    pub(crate) flags: Vec<OsString>,
    /// With a project config: the doc's place under the private directory, mirroring its place
    /// under the config's directory, which the block's virtual path is built from so relative
    /// globs resolve as they do in the project. `None` when there is no project config.
    pub(crate) mirror: Option<PathBuf>,
    /// With a project config: how to name the private paths in biome's messages: the private
    /// file and directory, with the project's file and directory they stand for.
    pub(crate) names: Option<[(PathBuf, PathBuf); 2]>,
}

impl Given {
    /// `stderr` with each private path biome printed named as the project's path it stands for.
    #[must_use]
    pub(crate) fn named(&self, stderr: &[u8]) -> Vec<u8> {
        let Some(names) = &self.names else {
            return stderr.to_vec();
        };
        let mut text = String::from_utf8_lossy(stderr).into_owned();
        for (private, project) in names {
            text = text.replace(&*private.to_string_lossy(), &project.to_string_lossy());
        }
        text.into_bytes()
    }
}

/// What biome is given for `doc`: the project's config, found walking up from `doc`'s directory
/// to `root`, parsed and validated by supdoc and re-serialized as strict JSON into a private
/// file, so biome never reads the project's bytes. Each setting that names a file biome reads
/// is rewritten to its absolute path. With no project config the file holds `{}`.
///
/// Effect: reads file metadata and the config, resolves symlinks, and may create supdoc's
/// private directories and the config file in them.
///
/// # Errors
///
/// [`Refusal::Config`] when the config found is a link out of the root, isn't a readable
/// JSON(C) object of at most 64 KiB, extends anything, declares plugins, or names a file
/// outside the root; [`Refusal::NoPrivateDir`] when there is nowhere private to put the file.
pub(crate) fn project_args(env: &ToolEnv, root: &Path, doc: &Path) -> Result<Given, Refusal> {
    let found = find(root, doc);
    let strict = match &found {
        Some(found) => checked(root, found)?,
        None => EMPTY_CONFIG.to_owned(),
    };
    let config = write_private(env, root, &strict).ok_or(Refusal::NoPrivateDir)?;
    let mut flag = OsString::from("--config-path=");
    flag.push(&config);
    let project_dir = found.as_deref().and_then(Path::parent);
    let private_dir = config.parent().map(Path::to_path_buf);
    let mirror = project_dir
        .zip(private_dir.as_ref())
        .and_then(|(project, private)| Some(private.join(doc.strip_prefix(project).ok()?)));
    let names = found
        .as_deref()
        .zip(project_dir.zip(private_dir))
        .map(|(file, (dir, private))| [(config, file.to_path_buf()), (private, dir.to_path_buf())]);
    Ok(Given {
        flags: vec![flag],
        mirror,
        names,
    })
}

/// The first biome config in `doc`'s directory or an ancestor of it inside `root`.
fn find(root: &Path, doc: &Path) -> Option<PathBuf> {
    doc.parent()?
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .flat_map(|dir| CONFIG_FILES.iter().map(move |name| dir.join(name)))
        .find(|candidate| fs::symlink_metadata(candidate).is_ok())
}

/// The strict JSON of the safe config at `found`, a file of the project at `root`.
///
/// # Errors
///
/// [`Refusal::Config`] naming what is wrong with it.
fn checked(root: &Path, found: &Path) -> Result<String, Refusal> {
    let file = found
        .strip_prefix(root)
        .unwrap_or(found)
        .display()
        .to_string();
    let refuse = |why: &str, help: &str| Refusal::Config {
        file: file.clone(),
        why: why.to_owned(),
        help: help.to_owned(),
    };
    let unreadable = || {
        refuse(
            "couldn't be read",
            "make it a readable UTF-8 file inside the project",
        )
    };
    let real_root = root.canonicalize().map_err(|_| unreadable())?;
    let real = found.canonicalize().map_err(|_| unreadable())?;
    if !real.starts_with(&real_root) {
        return Err(refuse(
            "is a link that resolves outside the project",
            "replace it with a regular file inside the project",
        ));
    }
    let meta = fs::metadata(&real).map_err(|_| unreadable())?;
    if !meta.is_file() || meta.len() > MAX_CONFIG {
        return Err(refuse(
            "is too large or isn't a regular file (the limit is 64 KiB)",
            "make it a regular file of at most 64 KiB",
        ));
    }
    let text = fs::read_to_string(&real).map_err(|_| unreadable())?;
    let Ok(Value::Object(config)) = serde_json::from_str(&strip_jsonc(&text)) else {
        return Err(refuse(
            "doesn't parse as a JSON or JSONC object",
            "fix its syntax so it is one JSON object",
        ));
    };
    let mut value = Value::Object(config);
    if let Some(unsafe_key) = unsafe_part(&value) {
        return Err(match unsafe_key {
            "plugins" => refuse(
                "declares `plugins`, which biome would load and run",
                "remove `plugins` from it, or mark the blocks `ignore(<why>)`",
            ),
            _ => refuse(
                "`extends` another config, which supdoc doesn't follow",
                "remove `extends` from it, or mark the blocks `ignore(<why>)`",
            ),
        });
    }
    let dir = found.parent().unwrap_or(root);
    if let Err((setting, why)) = absolutize(&mut value, "", dir, &real_root) {
        return Err(refuse(
            &format!("sets `{setting}`, which {why}"),
            &format!(
                "point `{setting}` at an existing file or directory inside the project, or mark the blocks `ignore(<why>)`"
            ),
        ));
    }
    serde_json::to_string(&value).map_err(|_| unreadable())
}

/// Rewrites each setting in `value`, found under the key `parent`, that names something biome
/// reads from disk to its absolute path, resolved from the config's directory `dir`: biome
/// 2.6.5's `tailwind.stylesheet` (a file) and `vcs.root` (a directory). Its schema holds no
/// other path-valued setting, and `formatter.useEditorconfig` reads an `.editorconfig` from
/// beside the config, which the private copy doesn't have, so it can't be honored.
///
/// # Errors
///
/// The setting's name and why it isn't usable: it resolves outside `root`, doesn't exist, or
/// asks for the `.editorconfig`.
fn absolutize(
    value: &mut Value,
    parent: &str,
    dir: &Path,
    root: &Path,
) -> Result<(), (String, &'static str)> {
    match value {
        Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                let named = matches!(
                    (parent, key.as_str()),
                    ("tailwind", "stylesheet") | ("vcs", "root")
                );
                if key == "useEditorconfig" && *inner == Value::Bool(true) {
                    return Err((
                        format!("{parent}.{key}"),
                        "loads an `.editorconfig` from beside the config, which supdoc can't give biome",
                    ));
                }
                match inner {
                    Value::String(path) if named => {
                        let real = dir.join(&*path).canonicalize().map_err(|_| {
                            (format!("{parent}.{key}"), "names a path that doesn't exist")
                        })?;
                        if !real.starts_with(root) {
                            return Err((
                                format!("{parent}.{key}"),
                                "names a path outside the project",
                            ));
                        }
                        *path = real.to_string_lossy().into_owned();
                    }
                    other => absolutize(other, key, dir, root)?,
                }
            }
            Ok(())
        }
        Value::Array(items) => items
            .iter_mut()
            .try_for_each(|item| absolutize(item, parent, dir, root)),
        _ => Ok(()),
    }
}

/// The first key in `value`, at any depth, biome mustn't be given: a non-empty `plugins` or
/// `extends`.
fn unsafe_part(value: &Value) -> Option<&'static str> {
    match value {
        Value::Object(map) => ["plugins", "extends"]
            .into_iter()
            .find(|key| {
                map.get(*key).is_some_and(|value| match value {
                    Value::Null => false,
                    Value::Array(items) => !items.is_empty(),
                    Value::String(name) => !name.is_empty(),
                    Value::Object(items) => !items.is_empty(),
                    _ => true,
                })
            })
            .or_else(|| map.values().find_map(unsafe_part)),
        Value::Array(items) => items.iter().find_map(unsafe_part),
        _ => None,
    }
}

/// `text`, JSON with comments and trailing commas, as plain JSON.
fn strip_jsonc(text: &str) -> String {
    without_trailing_commas(&without_comments(text))
}

/// `text` with its `//` and `/* */` comments removed.
fn without_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push(c);
                while let Some(inner) = chars.next() {
                    out.push(inner);
                    match inner {
                        '\\' => out.extend(chars.next()),
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                for skipped in chars.by_ref() {
                    if skipped == '\n' || skipped == '\r' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                let _opener = chars.next();
                let mut last = ' ';
                for skipped in chars.by_ref() {
                    if last == '*' && skipped == '/' {
                        break;
                    }
                    last = skipped;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// `text` with each comma that only precedes a closing `}` or `]` removed.
fn without_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        out.push(c);
        match c {
            '"' => {
                while let Some(inner) = chars.next() {
                    out.push(inner);
                    match inner {
                        '\\' => out.extend(chars.next()),
                        '"' => break,
                        _ => {}
                    }
                }
            }
            ',' => {
                let next = chars.clone().find(|c| !c.is_whitespace());
                if matches!(next, Some('}' | ']')) {
                    let _comma = out.pop();
                }
            }
            _ => {}
        }
    }
    out
}

/// A private file holding `strict`, named by the hash of its bytes, under a `biome-config`
/// directory beside the empty working directory tools run in. `None` when there is no private
/// directory or it fails [`private_dir`].
///
/// Effect: creates the directories mode 0700 and writes the file mode 0600.
fn write_private(env: &ToolEnv, root: &Path, strict: &str) -> Option<PathBuf> {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let neutral = runner::neutral_dir(env, root);
    if neutral == Path::new("/") {
        return None;
    }
    let base = private_dir(&neutral.parent()?.join("biome-config"))?;
    let name = Sha256::digest(strict.as_bytes())
        .iter()
        .fold(String::new(), |mut name, byte| {
            let _written = write!(name, "{byte:02x}");
            name
        });
    let dir = private_dir(&base.join(name))?;
    let file = dir.join("biome.json");
    if fs::symlink_metadata(&file).is_ok_and(|meta| meta.is_file())
        && fs::read_to_string(&file).is_ok_and(|text| text == strict)
    {
        return Some(file);
    }
    let _stale = fs::remove_file(&file);
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)
        .ok()?;
    out.write_all(strict.as_bytes()).ok()?;
    Some(file)
}

/// `dir`, created mode 0700 when missing, when it is a real directory (not a link) only its
/// owner reaches.
fn private_dir(dir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .ok()?;
    let meta = fs::symlink_metadata(dir).ok()?;
    (meta.is_dir() && meta.permissions().mode() & 0o777 == 0o700).then(|| dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{Refusal, extension, parse_error, project_args};
    use crate::code_blocks::format::ParseError;
    use crate::code_blocks::tools::ToolEnv;
    use std::fs;

    #[test]
    fn a_tag_picks_the_extension_biome_reads_the_block_by() {
        let picked: Vec<_> = [
            "ts",
            "typescript",
            "mts",
            "cts",
            "tsx",
            "js",
            "javascript",
            "mjs",
            "cjs",
            "jsx",
        ]
        .map(extension)
        .into();

        assert_eq!(
            picked,
            [
                "ts", "ts", "mts", "cts", "tsx", "js", "js", "mjs", "cjs", "jsx"
            ]
        );
    }

    #[test]
    fn a_parse_header_names_the_zero_based_body_line() {
        let stderr = "/x/doc.md.1.ts:3:11 parse ━━━━━━━━\n\n  × Expected an expression\n";

        assert_eq!(parse_error(stderr), Some(ParseError { line: Some(2) }));
    }

    #[test]
    fn other_stderr_is_no_parse_error() {
        assert_eq!(parse_error("biome.json configuration ━━━\n  × bad\n"), None);
    }

    /// The config supdoc gives biome for a root holding `config`, as a project at `root` with
    /// `HOME` set to a directory outside it.
    fn given(config: &str) -> Result<String, Refusal> {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let home = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::write(root.path().join("biome.jsonc"), config).expect("the config");
        let env = ToolEnv::from_vars(|name| (name == "HOME").then(|| home.path().into()));
        let args = project_args(&env, root.path(), &root.path().join("a.md"))?;
        let flag = args
            .flags
            .first()
            .expect("one flag")
            .to_string_lossy()
            .into_owned();
        let path = flag.strip_prefix("--config-path=").expect("the flag");
        Ok(fs::read_to_string(path).expect("the written config"))
    }

    #[test]
    fn a_config_biome_would_read_differently_is_never_given_to_biome_as_written() {
        for config in [
            "{\"formatter\":{} // x, \"plugins\": [\"./p.js\"]\r}",
            "{\"formatter\":{} // x, \"plugins\": [\"./p.js\"]\r\n}",
            "{\"formatter\":{} // x, \"plugins\": [\"./p.js\"]\u{2028}}",
            "{\"formatter\":{} /* x */, \"plugins\": [\"./p.js\"]}",
            "{\"a\": 1,\r\"plugins\": [\"./p.js\"]}",
        ] {
            match given(config) {
                Err(_) => {}
                Ok(written) => assert!(
                    !written.contains("plugins") && !written.contains("p.js"),
                    "{config:?} gave biome {written}"
                ),
            }
        }
    }

    #[test]
    fn what_biome_is_given_is_strict_json_without_comments() {
        let written = given("{ // c\n \"formatter\": {\"indentWidth\": 4,}, /* d */ }")
            .expect("a plain config is given");

        assert_eq!(written, "{\"formatter\":{\"indentWidth\":4}}");
    }

    #[test]
    fn plugins_and_extends_at_any_depth_are_refused_with_their_own_help() {
        let plugins = given("{\"overrides\": [{\"plugins\": [\"x.grit\"]}]}");
        let extends = given("{\"extends\": [\"./b.json\"]}");
        let broken = given("{\"a\": ");
        let empty = given("{\"extends\": [], \"plugins\": []}");

        let help = |refused: Result<String, Refusal>| match refused {
            Err(Refusal::Config { help, .. }) => help,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert!(help(plugins).contains("plugins"));
        assert!(help(extends).contains("extends"));
        assert!(help(broken).contains("syntax"));
        assert_eq!(empty, Ok("{\"extends\":[],\"plugins\":[]}".to_owned()));
    }

    #[test]
    fn a_parse_header_naming_a_config_is_not_a_block_parse_error() {
        let stderr = "/p/biome.json:2:3 parse ━━━━\n  × bad\n";

        assert_eq!(parse_error(stderr), None);
    }

    #[test]
    fn the_nearest_config_inside_the_root_is_used_and_an_unsafe_one_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let home = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::create_dir_all(root.path().join("docs/deep")).expect("the fixture dirs");
        fs::write(
            root.path().join("biome.json"),
            "{\"plugins\": [\"a.grit\"]}",
        )
        .expect("the root config");
        fs::write(root.path().join("docs/biome.jsonc"), "{ // ok\n}").expect("the docs config");
        let env = ToolEnv::from_vars(|name| (name == "HOME").then(|| home.path().into()));

        let passed = project_args(&env, root.path(), &root.path().join("docs/deep/a.md"));
        let refused = project_args(&env, root.path(), &root.path().join("a.md"));

        assert!(passed.is_ok(), "got {passed:?}");
        assert!(
            matches!(refused, Err(Refusal::Config { ref file, .. }) if file == "biome.json"),
            "got {refused:?}"
        );
    }

    #[test]
    fn a_comment_ended_by_a_bare_cr_leaves_the_key_after_it_in_the_written_config() {
        let written = given("{ // c\r \"formatter\": {\"indentWidth\": 4}}")
            .expect("a plain config is given");

        assert_eq!(written, "{\"formatter\":{\"indentWidth\":4}}");
    }

    #[test]
    fn the_exploit_shape_a_cr_ended_comment_before_plugins_is_refused() {
        let refused = given("{\"formatter\":{} // x\r, \"plugins\": [\"./p.js\"]\n}");

        assert!(
            matches!(refused, Err(Refusal::Config { ref why, .. }) if why.contains("plugins")),
            "got {refused:?}"
        );
    }

    /// `project_args` for `doc` in a project holding `files`, as the project root's tempdir and
    /// the result.
    fn project_with(
        files: &[(&str, &str)],
        doc: &str,
    ) -> (tempfile::TempDir, Result<super::Given, Refusal>) {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let home = tempfile::TempDir::new().expect("a temp dir should be creatable");
        for (name, text) in files {
            let path = root.path().join(name);
            fs::create_dir_all(path.parent().expect("a parent")).expect("the fixture dirs");
            fs::write(path, text).expect("the fixture file");
        }
        let env = ToolEnv::from_vars(|name| (name == "HOME").then(|| home.path().into()));
        let given = project_args(&env, root.path(), &root.path().join(doc));
        // The home must outlive the written config's use below.
        std::mem::forget(home);
        (root, given)
    }

    #[test]
    fn a_docs_place_under_the_private_dir_mirrors_its_place_under_the_configs_dir() {
        let (_root, given) = project_with(
            &[("pkg/biome.json", "{}"), ("pkg/docs/a.md", "")],
            "pkg/docs/a.md",
        );

        let given = given.expect("a plain config is given");

        let mirror = given.mirror.expect("a project config gives a mirror");
        assert!(mirror.ends_with("docs/a.md"), "got {mirror:?}");
        assert!(!mirror.to_string_lossy().contains("pkg"), "got {mirror:?}");
    }

    #[test]
    fn a_relative_stylesheet_is_written_absolute_and_one_outside_the_project_is_refused() {
        let (root, inside) = project_with(
            &[
                (
                    "biome.json",
                    "{\"tailwind\":{\"stylesheet\":\"./docs/app.css\"}}",
                ),
                ("docs/app.css", ""),
            ],
            "docs/a.md",
        );
        let (_other, outside) = project_with(
            &[(
                "biome.json",
                "{\"tailwind\":{\"stylesheet\":\"../app.css\"}}",
            )],
            "a.md",
        );

        let flag = inside.expect("an inside stylesheet is given").flags;
        let flag = flag
            .first()
            .expect("one flag")
            .to_string_lossy()
            .into_owned();
        let written = fs::read_to_string(flag.strip_prefix("--config-path=").expect("the flag"))
            .expect("the written config");
        let css = root
            .path()
            .canonicalize()
            .expect("the root resolves")
            .join("docs/app.css");
        assert_eq!(
            written,
            format!(
                "{{\"tailwind\":{{\"stylesheet\":{:?}}}}}",
                css.display().to_string()
            )
        );
        assert!(
            matches!(outside, Err(Refusal::Config { ref why, .. }) if why.contains("tailwind.stylesheet")),
            "got {outside:?}"
        );
    }

    #[test]
    fn a_config_that_loads_an_editorconfig_is_refused_naming_the_setting() {
        let refused = given("{\"formatter\": {\"useEditorconfig\": true}}");

        assert!(
            matches!(refused, Err(Refusal::Config { ref why, .. }) if why.contains("formatter.useEditorconfig")),
            "got {refused:?}"
        );
    }

    #[test]
    fn biomes_messages_name_the_projects_config_not_the_private_copy() {
        let (root, given) = project_with(&[("biome.json", "{}")], "a.md");
        let given = given.expect("a plain config is given");
        let private = given.mirror.clone().unwrap_or_default();
        let flag = given
            .flags
            .first()
            .expect("one flag")
            .to_string_lossy()
            .into_owned();
        let config = flag
            .strip_prefix("--config-path=")
            .expect("the flag")
            .to_owned();

        let named = given.named(format!("{config}:1:2 bad\n{}", private.display()).as_bytes());

        let text = String::from_utf8(named).expect("UTF-8");
        assert!(
            text.contains(&root.path().join("biome.json").display().to_string()),
            "{text}"
        );
        assert!(!text.contains("biome-config"), "{text}");
    }

    #[test]
    fn with_no_config_and_no_private_dir_the_block_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");

        let refused = project_args(&ToolEnv::default(), root.path(), &root.path().join("a.md"));

        assert_eq!(refused.err(), Some(Refusal::NoPrivateDir));
    }
}
