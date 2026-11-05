//! Reading a `from(...)` or `uses(...)` source: its path resolved and contained, then the whole
//! file or one symbol's text read.
//!
//! A path follows the link path grammar ([`split_namespace`], [`percent_decode`]): it is relative to
//! the doc's directory, to the project root when it starts with `/`, or to a namespace's directory
//! when written `ns:path` with no `/` before the `:`, and `%XX` escapes are decoded. After lexical normalization it may not climb
//! out of the root (or the namespace), and the file it names must lie inside the root once
//! symlinks are followed. A source outside is refused before it is opened, so nothing of it is
//! read or shown.

use std::path::Path;

use crate::cartridges::data::KeyPathBudget;
use crate::config::Config;
use crate::finding::{Finding, Location, Severity};
use crate::hash::Hash;
use crate::namespace::{NamespaceName, unknown_namespace_help};
use crate::output::paths::relative_to;
use crate::path::{AbsolutePath, percent_decode, split_namespace};
use crate::read::cartridge::{Languages, SourceFile, Symbol};
use crate::read::error::{CartridgeLoadError, ReadError, ReadRole};
use crate::read::hash::hash_symbol;

use super::info::SourceKind;

/// The code for a `from(...)` or `uses(...)` value that doesn't resolve.
pub(crate) const SOURCE_MISSING: &str = "code-block-source-missing";

/// Why a source value doesn't resolve, as a finding's message and help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Missing {
    /// What is wrong, naming the value as written or the file relative to the root.
    message: String,
    /// What to do next.
    help: String,
}

impl Missing {
    /// A failure with `message` and `help`.
    fn new(message: impl Into<String>, help: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            help: help.into(),
        }
    }

    /// The `code-block-source-missing` error at `at`, the value as written. It has no fix.
    #[must_use]
    pub(crate) fn finding(self, at: Location) -> Finding {
        Finding::new(SOURCE_MISSING, Severity::Error, self.message)
            .with_location(at)
            .with_help(self.help)
    }
}

/// Why a source value doesn't resolve.
#[derive(Debug)]
pub(crate) enum Unresolved {
    /// The value is wrong: one `code-block-source-missing` error at it.
    Missing(Missing),
    /// A built-in cartridge doesn't load: a supdoc bug that fails the whole run with
    /// `read-cartridge-load` (exit 70), never a finding about the value.
    CartridgeLoad(CartridgeLoadError),
}

impl From<Missing> for Unresolved {
    fn from(missing: Missing) -> Self {
        Self::Missing(missing)
    }
}

/// The text a source value names, and the indentation its first line has in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceText {
    /// The whole file, or the symbol's bytes.
    pub(crate) text: String,
    /// The whitespace before the symbol on its first line in the file; empty for a whole file.
    pub(crate) indent: String,
}

/// The text the `kind` attribute's `value` names: the whole file for `PATH`, or the symbol's
/// bytes for `PATH#SYMBOL`. `doc` is the doc holding the block, relative to the project root
/// with `/` separators.
///
/// Effect: follows symlinks along the path, then reads and parses the file under
/// `parse-timeout`. A file outside the root is never opened.
///
/// # Errors
///
/// [`Unresolved::CartridgeLoad`] when the source's built-in cartridge doesn't load. Otherwise
/// [`Unresolved::Missing`] for an unknown namespace, a URL, a path that climbs out of the root or its
/// namespace, a symlink out of the root, a `uses(...)` without a symbol, a missing or unreadable file, a directory, a file no
/// cartridge reads, a timeout, and a missing, ambiguous or malformed symbol.
pub(crate) fn read(
    config: &Config,
    languages: &Languages,
    doc: &str,
    kind: SourceKind,
    value: &str,
) -> Result<SourceText, Unresolved> {
    let (path, symbol) = split_value(value);
    if kind == SourceKind::Uses && symbol.is_none() {
        return Err(symbolless(value));
    }
    let located = locate(config, doc, kind, path)?;
    resolve(
        config,
        languages,
        &located,
        kind,
        value,
        symbol,
        |source, found| match found {
            None => Some(SourceText {
                text: source.text().to_owned(),
                indent: String::new(),
            }),
            Some(found) => symbol_text(source.text(), found.range()),
        },
    )
}

/// What a `uses(...)` value names: its target as the lockfile keys it, and that symbol's hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Use {
    /// `path#symbol` or `ns:path#symbol`, the path relative to the project root or to the
    /// namespace's directory, with `/` separators.
    pub(crate) target: String,
    /// The symbol's hash under the config's `[features.hash]` settings.
    pub(crate) hash: Hash,
}

/// The target and current hash of the symbol a `uses(...)` `value` names. `doc` is the doc
/// holding the block, relative to the project root with `/` separators.
///
/// Effect: as [`read`].
///
/// # Errors
///
/// As [`read`].
pub(crate) fn read_use(
    config: &Config,
    languages: &Languages,
    doc: &str,
    value: &str,
) -> Result<Use, Unresolved> {
    let kind = SourceKind::Uses;
    let (path, Some(symbol)) = split_value(value) else {
        return Err(symbolless(value));
    };
    let located = locate(config, doc, kind, path)?;
    let settings = config.hash_settings();
    let hash = resolve(
        config,
        languages,
        &located,
        kind,
        value,
        Some(symbol),
        |source, found| found.and_then(|found| hash_symbol(source, found, settings).ok()),
    )?;
    Ok(Use {
        target: format!("{}#{symbol}", located.target),
        hash,
    })
}

/// The failure for a `uses(...)` `value` without a symbol.
fn symbolless(value: &str) -> Unresolved {
    Unresolved::Missing(Missing::new(
        format!("`uses({value})` names no symbol"),
        format!("write it as `uses({value}#SYMBOL)`; `uses(...)` tracks one symbol"),
    ))
}

/// `value` split at its first `#` into the path and the symbol, if any.
fn split_value(value: &str) -> (&str, Option<&str>) {
    match value.split_once('#') {
        Some((path, symbol)) => (path, Some(symbol)),
        None => (value, None),
    }
}

/// Reads and parses `located`'s file, resolves `symbol` in it, and gives both to `take`. A
/// `None` from `take` means the symbol doesn't fit the file: a supdoc bug, reported as missing.
///
/// Effect: reads and parses the file under `parse-timeout`.
///
/// # Errors
///
/// As [`read`].
fn resolve<T>(
    config: &Config,
    languages: &Languages,
    located: &Located,
    kind: SourceKind,
    value: &str,
    symbol: Option<&str>,
    take: impl FnOnce(&SourceFile, Option<&Symbol>) -> Option<T>,
) -> Result<T, Unresolved> {
    let name = kind.name();
    let limits = config.limits_in_effect();
    let source = SourceFile::read(
        located.file.clone(),
        languages,
        limits.parse_timeout.value,
        KeyPathBudget::from_limits(limits),
    )
    .map_err(|error| read_failed(&error, config.root(), kind))?;
    let shown = relative_to(config.root(), source.path());
    let outside = || {
        Unresolved::Missing(Missing::new(
            format!("the symbol in `{name}({value})` lies outside {shown} (supdoc bug)"),
            "report this at https://github.com/aidenzepp/supdoc/issues",
        ))
    };
    let Some(symbol) = symbol else {
        return take(&source, None).ok_or_else(outside);
    };
    let table = source
        .language()
        .symbols(&source)
        .map_err(|error| read_failed(&error, config.root(), kind))?;
    match table.resolve_text(source.path(), symbol) {
        None => Err(Unresolved::Missing(Missing::new(
            format!("`{symbol}` in `{name}({value})` isn't a symbol path"),
            "name a symbol by its dotted path, such as `Type.method`, with no empty part",
        ))),
        Some(Err(error)) => Err(Unresolved::Missing(Missing::new(
            error.message_naming(&shown),
            error.help_from_root(&shown),
        ))),
        Some(Ok(found)) => take(&source, Some(found)).ok_or_else(outside),
    }
}

/// The bytes of `range` in `text`, with the whitespace that starts `range`'s line before it.
/// `None` when `range` doesn't lie on character boundaries inside `text`.
fn symbol_text(text: &str, range: std::ops::Range<usize>) -> Option<SourceText> {
    let line_start = text
        .get(..range.start)?
        .rfind('\n')
        .map_or(0, |found| found + 1);
    let before = text.get(line_start..range.start)?;
    let indent_len = before.len() - before.trim_start_matches([' ', '\t']).len();
    Some(SourceText {
        text: text.get(range)?.to_owned(),
        indent: before.get(..indent_len)?.to_owned(),
    })
}

/// A source path resolved and contained, before its file is opened.
struct Located {
    /// The file.
    file: AbsolutePath,
    /// The file as the lockfile names a target: relative to the project root, or `ns:path`
    /// relative to the namespace's directory.
    target: String,
}

/// The file `path` names, contained in the project root, without opening it.
///
/// # Errors
///
/// A [`Missing`] for a URL, an unknown namespace, a climb out of the root or the namespace, or a
/// file whose real location is outside the root.
fn locate(config: &Config, doc: &str, kind: SourceKind, path: &str) -> Result<Located, Missing> {
    let name = kind.name();
    if path.contains("://") {
        return Err(Missing::new(
            format!("`{name}({path})` is a URL"),
            format!("`{name}(...)` reads a file in this project: write its path, not a URL"),
        ));
    }
    let root = config.root();
    let (prefix, written) = match split_namespace(path) {
        Some((prefix, rest)) => (Some(prefix), rest),
        None => (None, path),
    };
    let Some(decoded) = percent_decode(written) else {
        return Err(Missing::new(
            format!("`{name}({path})` isn't valid UTF-8 after percent-decoding"),
            "write the path's non-ASCII characters as they are, or as `%XX` escapes of UTF-8",
        ));
    };
    let (boundary, base, rest) = if let Some(prefix) = prefix {
        let directory = prefix
            .parse::<NamespaceName>()
            .ok()
            .and_then(|namespace| config.namespace(&namespace));
        let Some(directory) = directory else {
            let known: Vec<&str> = config
                .namespaces()
                .keys()
                .map(NamespaceName::as_str)
                .collect();
            return Err(Missing::new(
                format!("`{name}({path})` names the unknown namespace `{prefix}`"),
                unknown_namespace_help(prefix, &known),
            ));
        };
        (directory.clone(), directory.clone(), decoded.as_str())
    } else if let Some(rest) = decoded.strip_prefix('/') {
        (root.clone(), root.clone(), rest)
    } else {
        let doc_directory = root
            .join_path(Path::new(doc))
            .parent()
            .unwrap_or_else(|| root.clone());
        (root.clone(), doc_directory, decoded.as_str())
    };

    let file = base.join_path(Path::new(rest));
    if file.strip_prefix(&boundary).is_none() {
        return Err(Missing::new(
            format!(
                "`{name}({path})` climbs out of {}",
                boundary_name(&boundary, root)
            ),
            "name a file inside the project root; supdoc never reads a source outside it",
        ));
    }
    if !config.contains(boundary.as_path()) || !config.contains(file.as_path()) {
        return Err(Missing::new(
            format!("`{name}({path})` is outside the project root"),
            "a symlink along the path leads out of the project root; name a file inside it",
        ));
    }
    let inside = relative_to(&boundary, &file);
    let target = match prefix {
        Some(prefix) => format!("{prefix}:{inside}"),
        None => inside,
    };
    Ok(Located { file, target })
}

/// How a message names `boundary`: the project root, or a namespace's directory by its path.
fn boundary_name(boundary: &AbsolutePath, root: &AbsolutePath) -> String {
    if boundary == root {
        "the project root".to_owned()
    } else {
        format!("its namespace directory `{}`", relative_to(root, boundary))
    }
}

/// Why a source file can't be read, naming it relative to `root`. A cartridge that doesn't load
/// is [`Unresolved::CartridgeLoad`], never a finding.
fn read_failed(error: &ReadError, root: &AbsolutePath, kind: SourceKind) -> Unresolved {
    if let ReadError::CartridgeLoad { error, .. } = error {
        return Unresolved::CartridgeLoad(*error);
    }
    let shown = relative_to(root, error.path());
    let name = kind.name();
    let help = match error {
        ReadError::Missing { .. } => format!(
            "check the path in `{name}(...)`: it is relative to this doc's directory, to the \
             project root when it starts with `/`, or to a namespace when written `ns:path`"
        ),
        ReadError::NotAFile { .. } => {
            format!("`{shown}` is a directory or special file; name a regular file")
        }
        _ => error.help(ReadRole::ResolveArgument, &shown),
    };
    Unresolved::Missing(Missing::new(error.message_naming(&shown), help))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;

    /// The library every test reads from: `add` and `Limits.resolve`.
    const LIB: &str =
        "pub fn add() {}\n\npub struct Limits;\n\nimpl Limits {\n    pub fn resolve() {}\n}\n";

    /// A project with `config` as its config, `src/lib.rs` holding [`LIB`], and a `docs/`
    /// directory, plus a secret file in a separate directory outside it.
    struct Fixture {
        /// The project root.
        root: TempDir,
        /// A directory outside the root holding `secret.rs`.
        outside: TempDir,
    }

    impl Fixture {
        fn new(config: &str) -> Self {
            let root = TempDir::new().expect("a temp root should be creatable");
            let outside = TempDir::new().expect("a temp outside dir should be creatable");
            fs::write(root.path().join("supdoc.config.toml"), config)
                .expect("the config should be writable");
            fs::create_dir_all(root.path().join("src")).expect("src should be creatable");
            fs::create_dir_all(root.path().join("docs")).expect("docs should be creatable");
            fs::write(root.path().join("src/lib.rs"), LIB).expect("the lib should be writable");
            fs::write(outside.path().join("secret.rs"), "pub fn secret() {}\n")
                .expect("the secret should be writable");
            Self { root, outside }
        }

        fn config(&self) -> Config {
            Config::load(&AbsolutePath::new(self.root.path()).expect("a temp path is absolute"))
                .expect("the fixture config should load")
        }

        /// Reads `value` as `kind` for a block in `docs/guide.md`. A fixture never meets a
        /// cartridge that doesn't load.
        fn read(&self, kind: SourceKind, value: &str) -> Result<String, Missing> {
            self.read_text(kind, value).map(|found| found.text)
        }

        /// Like [`read`](Self::read), keeping the symbol's indentation.
        fn read_text(&self, kind: SourceKind, value: &str) -> Result<SourceText, Missing> {
            let result = read(
                &self.config(),
                Languages::builtin().expect("the built-in cartridges should load"),
                "docs/guide.md",
                kind,
                value,
            );
            result.map_err(|unresolved| match unresolved {
                Unresolved::Missing(missing) => missing,
                Unresolved::CartridgeLoad(error) => panic!("a built-in cartridge failed: {error}"),
            })
        }
    }

    #[test]
    fn a_relative_path_resolves_from_the_doc_directory() {
        let fixture = Fixture::new("");

        let text = fixture.read(SourceKind::From, "../src/lib.rs#add");

        assert_eq!(text.as_deref(), Ok("pub fn add() {}"));
    }

    #[test]
    fn a_leading_slash_resolves_from_the_root_and_a_bare_path_reads_the_whole_file() {
        let fixture = Fixture::new("");

        let method = fixture.read(SourceKind::Uses, "/src/lib.rs#Limits.resolve");
        let whole = fixture.read(SourceKind::From, "/src/lib.rs");

        assert_eq!(method.as_deref(), Ok("pub fn resolve() {}"));
        assert_eq!(whole.as_deref(), Ok(LIB));
    }

    #[test]
    fn a_namespace_resolves_from_its_directory_and_an_unknown_one_fails() {
        let fixture = Fixture::new("[namespaces]\ncore = \"src\"\n");

        let found = fixture.read(SourceKind::From, "core:lib.rs#add");
        let unknown = fixture.read(SourceKind::From, "nope:lib.rs#add");

        assert_eq!(found.as_deref(), Ok("pub fn add() {}"));
        assert!(unknown.is_err(), "{unknown:?}");
    }

    #[test]
    fn a_colon_after_a_slash_is_part_of_the_path_not_a_namespace() {
        let fixture = Fixture::new("[namespaces]\nsrc = \"src\"\n");
        fs::create_dir_all(fixture.root.path().join("docs/src"))
            .expect("docs/src should be creatable");
        fs::write(
            fixture.root.path().join("docs/src/a:b.rs"),
            "pub fn ab() {}\n",
        )
        .expect("the colon file should be writable");

        let found = fixture.read(SourceKind::From, "src/a:b.rs#ab");

        assert_eq!(found.as_deref(), Ok("pub fn ab() {}"));
    }

    #[test]
    fn a_percent_escape_is_decoded_as_a_link_decodes_it() {
        let fixture = Fixture::new("");
        fs::write(fixture.root.path().join("docs/a b.rs"), "pub fn ab() {}\n")
            .expect("the spaced file should be writable");

        let found = fixture.read(SourceKind::From, "a%20b.rs#ab");

        assert_eq!(found.as_deref(), Ok("pub fn ab() {}"));
    }

    #[test]
    fn a_method_keeps_the_indentation_of_its_first_line() {
        let fixture = Fixture::new("");

        let method = fixture.read_text(SourceKind::From, "/src/lib.rs#Limits.resolve");

        assert_eq!(
            method.map(|found| found.indent).as_deref(),
            Ok("    "),
            "a method in an impl is indented four spaces"
        );
    }

    #[test]
    fn a_climb_out_of_the_root_or_a_namespace_is_refused() {
        let fixture = Fixture::new("[namespaces]\ncore = \"src\"\n");

        for value in [
            "../../outside.rs",
            "/../outside.rs",
            "core:../docs/guide.md",
            "core:/etc/hosts",
        ] {
            let result = fixture.read(SourceKind::From, value);

            assert!(
                result
                    .as_ref()
                    .is_err_and(|missing| missing.message.contains("climbs out")),
                "{value}: {result:?}"
            );
        }
    }

    #[test]
    fn an_in_root_symlink_to_an_outside_file_is_refused_without_a_read() {
        let fixture = Fixture::new("");
        symlink(
            fixture.outside.path().join("secret.rs"),
            fixture.root.path().join("src/link.rs"),
        )
        .expect("the symlink should be creatable");

        for kind in [SourceKind::From, SourceKind::Uses] {
            let result = fixture.read(kind, "/src/link.rs#secret");

            let missing = result.expect_err("an outside source should be refused");
            assert!(
                missing.message.contains("outside the project root"),
                "{missing:?}"
            );
            assert!(
                !missing.message.contains("secret()") && !missing.help.contains("secret()"),
                "{missing:?}"
            );
        }
    }

    #[test]
    fn a_missing_symbol_suggests_the_nearest_name() {
        let fixture = Fixture::new("");

        let missing = fixture
            .read(SourceKind::From, "/src/lib.rs#addd")
            .expect_err("a misspelled symbol should not resolve");

        assert!(missing.help.contains("`add`"), "{missing:?}");
    }

    #[test]
    fn a_url_a_directory_a_missing_file_and_a_symbolless_uses_fail() {
        let fixture = Fixture::new("");

        for (kind, value) in [
            (SourceKind::From, "https://example.com/lib.rs#add"),
            (SourceKind::From, "/src"),
            (SourceKind::From, "/src/gone.rs#add"),
            (SourceKind::Uses, "/src/lib.rs"),
            (SourceKind::From, "/supdoc.config.toml#add"),
        ] {
            let result = fixture.read(kind, value);

            assert!(result.is_err(), "{value}: {result:?}");
        }
    }

    #[test]
    fn a_cartridge_that_does_not_load_is_fatal_not_a_finding() {
        let grammar: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query_error = tree_sitter::Query::new(&grammar, "(not_a_node")
            .expect_err("an unclosed query should not compile");
        let broken: &'static crate::read::query::QueryFileError =
            Box::leak(Box::new(crate::read::query::QueryFileError {
                language: crate::read::cartridge::LanguageId::new("rust"),
                file: "tags.scm",
                error: query_error,
            }));
        let root = AbsolutePath::new(Path::new("/repo")).expect("the fixture root is absolute");
        let error = ReadError::CartridgeLoad {
            path: AbsolutePath::new(Path::new("/repo/src/lib.rs"))
                .expect("the fixture path is absolute"),
            error: CartridgeLoadError::Query(broken),
        };

        for kind in [SourceKind::From, SourceKind::Uses] {
            let unresolved = read_failed(&error, &root, kind);

            assert!(
                matches!(unresolved, Unresolved::CartridgeLoad(_)),
                "{unresolved:?}"
            );
        }
    }
}
