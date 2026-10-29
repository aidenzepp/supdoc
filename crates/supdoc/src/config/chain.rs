//! Finding a config, reading its `extends` chain and merging the layers.
//!
//! `discover`, `read_chain` and `confine_namespaces` are the only functions here that touch the
//! filesystem. `merge` reads only to find the root's real location.
//!
//! supdoc may run on an untrusted checkout whose config is hostile, so `extends` reads nothing but
//! a file named `supdoc.config.toml`, and a namespace may not name a directory outside the root.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, PathBuf};

use crate::path::{AbsolutePath, RelativePath};

use super::pattern::{AnchoredPatterns, PatternList};
use super::raw::RawConfig;
use super::{CONFIG_FILE_NAME, Config, ConfigError};

/// One config file in an `extends` chain, as parsed.
#[derive(Debug)]
pub(super) struct Layer {
    /// The file this layer was read from.
    file: AbsolutePath,
    /// Its parsed contents.
    raw: RawConfig,
}

/// The directory containing `file`, which every path in that file is relative to.
///
/// A file always has a parent. The fallback to `file` itself only keeps the function total for
/// the filesystem root, which cannot be a config file.
pub(super) fn directory_of(file: &AbsolutePath) -> AbsolutePath {
    file.parent().unwrap_or_else(|| file.clone())
}

/// `dir/supdoc.config.toml`, or `None` if that is somehow not absolute (it always is).
fn config_in(dir: &AbsolutePath) -> Option<AbsolutePath> {
    AbsolutePath::new(&dir.as_path().join(CONFIG_FILE_NAME))
}

/// Walks up from `start` to the nearest directory holding a config file (01 D1).
pub(super) fn discover(start: &AbsolutePath) -> Option<AbsolutePath> {
    let mut dir = Some(start.clone());
    while let Some(current) = dir {
        if let Some(candidate) = config_in(&current).filter(|file| file.as_path().is_file()) {
            return Some(candidate);
        }
        dir = current.parent();
    }
    None
}

/// Reads `file` and every config it extends, child first.
///
/// `file` must exist: a missing start file is [`ConfigError::NotFound`].
///
/// # Errors
///
/// Returns the first [`ConfigError`] found while reading, parsing or following `extends`.
pub(super) fn read_chain(file: &AbsolutePath) -> Result<Vec<Layer>, ConfigError> {
    let mut layers = Vec::new();
    // Each visited file with the name the filesystem knows it by, so a loop through a symlink
    // is still caught.
    let mut visited: Vec<(PathBuf, AbsolutePath)> = Vec::new();
    let mut next = Some(file.clone());
    while let Some(current) = next.take() {
        let text = read_text(&current, layers.is_empty())?;
        let raw: RawConfig = toml::from_str(&text).map_err(|error| ConfigError::Parse {
            path: current.clone(),
            error,
        })?;
        visited.push((identity(&current), current.clone()));
        if let Some(extends) = &raw.extends {
            let target = resolve_extends(&current, extends)?;
            if let Some(start) = visited
                .iter()
                .position(|(known, _)| *known == identity(&target))
            {
                let mut chain: Vec<AbsolutePath> = visited
                    .iter()
                    .skip(start)
                    .map(|(_, lexical)| lexical.clone())
                    .collect();
                chain.push(target);
                return Err(ConfigError::ExtendsLoop {
                    chain,
                    span: extends.span(),
                });
            }
            next = Some(target);
        }
        layers.push(Layer { file: current, raw });
    }
    Ok(layers)
}

/// The path the filesystem knows `file` by, falling back to its lexical form.
fn identity(file: &AbsolutePath) -> PathBuf {
    fs::canonicalize(file.as_path()).unwrap_or_else(|_| file.as_path().to_path_buf())
}

/// Reads a config file's text. The start file being absent is `NotFound`; any other file
/// was already confirmed to exist.
///
/// # Errors
///
/// `NotFound` for a missing start file, otherwise `Read` with the operating system's reason.
fn read_text(file: &AbsolutePath, is_start: bool) -> Result<String, ConfigError> {
    fs::read_to_string(file.as_path()).map_err(|error| {
        if is_start && error.kind() == ErrorKind::NotFound {
            ConfigError::NotFound { path: file.clone() }
        } else {
            ConfigError::Read {
                path: file.clone(),
                error,
            }
        }
    })
}

/// Resolves an `extends` value from the file that wrote it, checking it names a config file.
///
/// The value must be named `supdoc.config.toml`, and so must the file it really is once symlinks
/// are followed. Both are checked before anything is read, so a hostile `extends` can never make
/// supdoc print another file's lines in a parse error.
///
/// # Errors
///
/// `ExtendsNotConfig` for any other name, `ExtendsMissing` or `ExtendsDirectory` when the target
/// isn't a file, `Read` for other failures.
fn resolve_extends(
    from: &AbsolutePath,
    extends: &toml::Spanned<RelativePath>,
) -> Result<AbsolutePath, ConfigError> {
    let value = extends.get_ref();
    let resolved = directory_of(from).join(value);
    let directory = || ConfigError::ExtendsDirectory {
        file: from.clone(),
        extends: value.clone(),
        span: extends.span(),
        suggestion: config_inside(value, &resolved),
    };
    let not_config = || ConfigError::ExtendsNotConfig {
        file: from.clone(),
        extends: value.clone(),
        span: extends.span(),
    };
    match value.as_path().components().next_back() {
        None | Some(Component::CurDir | Component::ParentDir) => return Err(directory()),
        Some(Component::Normal(name)) if name == CONFIG_FILE_NAME => {}
        Some(_) => return Err(not_config()),
    }
    match fs::metadata(resolved.as_path()) {
        Ok(metadata) if metadata.is_dir() => Err(directory()),
        Ok(_) => match fs::canonicalize(resolved.as_path()) {
            Ok(real)
                if real
                    .file_name()
                    .is_some_and(|name| name == CONFIG_FILE_NAME) =>
            {
                Ok(resolved)
            }
            Ok(_) => Err(not_config()),
            Err(error) => Err(ConfigError::Read {
                path: resolved,
                error,
            }),
        },
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            Err(ConfigError::ExtendsMissing {
                file: from.clone(),
                extends: value.clone(),
                span: extends.span(),
                resolved,
            })
        }
        Err(error) => Err(ConfigError::Read {
            path: resolved,
            error,
        }),
    }
}

/// `extends` pointing at the config file inside `directory`, if there is one.
fn config_inside(extends: &RelativePath, directory: &AbsolutePath) -> Option<RelativePath> {
    if !config_in(directory).is_some_and(|file| file.as_path().is_file()) {
        return None;
    }
    let suggested = extends.as_path().join(CONFIG_FILE_NAME);
    RelativePath::try_from(suggested.to_string_lossy().into_owned()).ok()
}

/// Combines the layers, child first, into one config rooted at `root`.
///
/// Each key comes from the nearest layer that sets it, including each key under `[features]`. Namespaces merge by name, nearer wins.
/// Every path is anchored at the directory of the layer that wrote it.
pub(super) fn merge(root: AbsolutePath, layers: Vec<Layer>) -> Config {
    let mut files = Vec::with_capacity(layers.len());
    let mut include = None;
    let mut exclude = None;
    let mut namespaces = BTreeMap::new();
    let mut namespace_files = BTreeMap::new();
    let mut line_endings = None;
    for Layer { file, raw } in layers {
        let base = directory_of(&file);
        for (name, relative) in raw.namespaces {
            if let Entry::Vacant(slot) = namespaces.entry(name.clone()) {
                let _inserted = slot.insert(base.join(&relative));
                let _recorded = namespace_files.insert(name, file.clone());
            }
        }
        if include.is_none() {
            include = raw
                .include
                .map(|list| AnchoredPatterns::written_by(file.clone(), base.clone(), list));
        }
        if exclude.is_none() {
            exclude = raw
                .exclude
                .map(|list| AnchoredPatterns::written_by(file.clone(), base.clone(), list));
        }
        if line_endings.is_none() {
            line_endings = raw
                .features
                .and_then(|features| features.hash)
                .and_then(|hash| hash.line_endings)
                .map(|value| (value, file.clone()));
        }
        files.push(file);
    }
    let include = include
        .unwrap_or_else(|| AnchoredPatterns::built_in(root.clone(), PatternList::everything()));
    Config {
        real_root: super::real_root(&root),
        root,
        files,
        include,
        exclude,
        namespaces,
        namespace_files,
        line_endings,
    }
}

/// Checks every namespace names a directory inside the project root, once symlinks are followed.
///
/// A missing directory is still allowed (01 D7): where it would be is what counts.
///
/// # Errors
///
/// `NamespaceOutsideRoot` for the first namespace, by name, whose directory lies outside.
pub(super) fn confine_namespaces(config: &Config) -> Result<(), ConfigError> {
    let outside = config
        .namespaces()
        .iter()
        .find(|(_, directory)| !config.contains(directory.as_path()));
    let Some((name, directory)) = outside else {
        return Ok(());
    };
    Err(ConfigError::NamespaceOutsideRoot {
        file: config
            .namespace_defined_in(name)
            .cloned()
            .unwrap_or_else(|| config.root().clone()),
        name: name.clone(),
        directory: directory.clone(),
        root: config.root().clone(),
    })
}
