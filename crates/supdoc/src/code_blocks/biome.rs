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
use std::os::unix::fs::DirBuilderExt as _;
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
const EMPTY_CONFIG: &str = "{}\n";

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

/// `biome format`'s syntax-error report in `stderr`: a header line `<path>:L:C parse ━━━`. `None`
/// for any other stderr.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    let header = stderr.lines().find(|line| line.contains(" parse ━"))?;
    let (place, _) = header.split_once(" parse ")?;
    let mut parts = place.rsplitn(3, ':');
    let _column = parts.next()?.parse::<usize>().ok()?;
    let line = parts.next()?.parse::<usize>().ok()?;
    Some(ParseError {
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
        why: &'static str,
    },
    /// supdoc has no private directory to keep biome's empty config in.
    NoPrivateDir,
}

/// The flags that hand biome `doc`'s project config, found walking up from `doc`'s directory to
/// `root`, or supdoc's own empty config when there is none.
///
/// Effect: reads file metadata and the config, resolves symlinks, and may create supdoc's
/// private directory and the empty config in it.
///
/// # Errors
///
/// [`Refusal::Config`] when the config found is a link out of the root, isn't a readable JSON
/// file of at most 64 KiB, extends anything, or declares plugins; [`Refusal::NoPrivateDir`] when
/// there is no config and nowhere private to put the empty one.
pub(crate) fn project_args(
    env: &ToolEnv,
    root: &Path,
    doc: &Path,
) -> Result<Vec<OsString>, Refusal> {
    let config = match find(root, doc) {
        Some(found) => checked(root, &found)?,
        None => empty_config(env, root).ok_or(Refusal::NoPrivateDir)?,
    };
    let mut flag = OsString::from("--config-path=");
    flag.push(config);
    Ok(vec![flag])
}

/// The first biome config in `doc`'s directory or an ancestor of it inside `root`.
fn find(root: &Path, doc: &Path) -> Option<PathBuf> {
    doc.parent()?
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .flat_map(|dir| CONFIG_FILES.iter().map(move |name| dir.join(name)))
        .find(|candidate| fs::symlink_metadata(candidate).is_ok())
}

/// `found`'s real path when it is a safe config of the project at `root`.
///
/// # Errors
///
/// [`Refusal::Config`] naming what is wrong with it.
fn checked(root: &Path, found: &Path) -> Result<PathBuf, Refusal> {
    let file = found
        .strip_prefix(root)
        .unwrap_or(found)
        .display()
        .to_string();
    let refuse = |why| Refusal::Config {
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
        return Err(refuse("isn't a regular file of at most 64 KiB"));
    }
    let text = fs::read_to_string(&real).map_err(|_| refuse("couldn't be read as UTF-8"))?;
    match unsafe_part(&text) {
        Some(why) => Err(refuse(why)),
        None => Ok(real),
    }
}

/// What in the config `text` biome mustn't be given, `None` when nothing.
fn unsafe_part(text: &str) -> Option<&'static str> {
    let Ok(Value::Object(config)) = serde_json::from_str(&strip_jsonc(text)) else {
        return Some("doesn't parse as a JSON object");
    };
    let present = |value: Option<&Value>| match value {
        None | Some(Value::Null) => false,
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::String(name)) => !name.is_empty(),
        Some(_) => true,
    };
    if present(config.get("extends")) {
        return Some("extends another config, which supdoc doesn't follow");
    }
    let overrides = match config.get("overrides") {
        Some(Value::Array(items)) => items.as_slice(),
        _ => &[],
    };
    let declares_plugins = std::iter::once(&config)
        .chain(overrides.iter().filter_map(Value::as_object))
        .any(|scope| present(scope.get("plugins")));
    declares_plugins.then_some("declares plugins, which biome would load")
}

/// `text`, JSON with comments and trailing commas, as plain JSON.
fn strip_jsonc(text: &str) -> String {
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
                    if skipped == '\n' {
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
            ',' => {
                let mut ahead = chars.clone();
                let next = ahead.find(|c: &char| !c.is_whitespace());
                if !matches!(next, Some('}' | ']')) {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// The path of supdoc's own empty biome config, written when it is missing or differs: a
/// `biome-default` directory beside the empty working directory tools run in.
///
/// Effect: resolves the working directory, creates `biome-default` mode 0700 and writes the file.
fn empty_config(env: &ToolEnv, root: &Path) -> Option<PathBuf> {
    let neutral = runner::neutral_dir(env, root);
    if neutral == Path::new("/") {
        return None;
    }
    let dir = neutral.parent()?.join("biome-default");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .ok()?;
    let file = dir.join("biome.json");
    let current = fs::symlink_metadata(&file)
        .ok()
        .filter(fs::Metadata::is_file)
        .and_then(|_| fs::read_to_string(&file).ok());
    if current.as_deref() != Some(EMPTY_CONFIG) {
        let _stale = fs::remove_file(&file);
        fs::write(&file, EMPTY_CONFIG).ok()?;
    }
    Some(file)
}

#[cfg(test)]
mod tests {
    use super::{Refusal, extension, parse_error, project_args, unsafe_part};
    use crate::code_blocks::format::ParseError;
    use crate::code_blocks::tools::ToolEnv;
    use std::ffi::OsString;
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

    #[test]
    fn comments_and_trailing_commas_do_not_hide_a_plugins_list() {
        let config = "{\n // note\n \"linter\": {\"enabled\": true,},\n /* x */ \"plugins\": [\"./a.grit\",],\n}";

        assert_eq!(
            unsafe_part(config),
            Some("declares plugins, which biome would load")
        );
    }

    #[test]
    fn extends_and_override_plugins_are_refused_but_empty_lists_are_not() {
        assert!(unsafe_part("{\"extends\": [\"@org/config\"]}").is_some());
        assert!(unsafe_part("{\"overrides\": [{\"plugins\": [\"x.grit\"]}]}").is_some());
        assert_eq!(unsafe_part("{\"extends\": [], \"plugins\": []}"), None);
        assert!(unsafe_part("[1]").is_some());
    }

    #[test]
    fn the_nearest_config_inside_the_root_is_passed_and_an_unsafe_one_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::create_dir_all(root.path().join("docs/deep")).expect("the fixture dirs");
        fs::write(
            root.path().join("biome.json"),
            "{\"plugins\": [\"a.grit\"]}",
        )
        .expect("the root config");
        fs::write(root.path().join("docs/biome.jsonc"), "{ // ok\n}").expect("the docs config");
        let env = ToolEnv::default();
        let near = root.path().join("docs/deep/a.md");
        let far = root.path().join("a.md");

        let passed = project_args(&env, root.path(), &near).expect("the docs config is safe");
        let refused = project_args(&env, root.path(), &far);

        let real = root
            .path()
            .canonicalize()
            .expect("the root resolves")
            .join("docs/biome.jsonc");
        let mut expected = OsString::from("--config-path=");
        expected.push(real);
        assert_eq!(passed, [expected]);
        assert!(
            matches!(refused, Err(Refusal::Config { ref file, .. }) if file == "biome.json"),
            "got {refused:?}"
        );
    }

    #[test]
    fn with_no_config_and_no_private_dir_the_block_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");

        let refused = project_args(&ToolEnv::default(), root.path(), &root.path().join("a.md"));

        assert_eq!(refused, Err(Refusal::NoPrivateDir));
    }
}
