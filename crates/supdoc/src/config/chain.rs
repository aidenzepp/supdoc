//! Finding a config, reading its `extends` chain and merging the layers.
//!
//! `discover`, `read_chain` and `confine_namespaces` are the only functions here that touch the
//! filesystem. `merge` reads only to find the root's real location.
//!
//! supdoc may run on an untrusted checkout whose config is hostile, so neither the config found by
//! walking up nor `extends` reads anything but a file named `supdoc.config.toml`, and a namespace
//! may not name a directory outside the root.

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
///
/// A directory holds one when its entry named `supdoc.config.toml` is a regular file or a
/// symlink, decided with `symlink_metadata` so the link is never followed to decide. Any other
/// entry, such as a real directory or a FIFO, is skipped, unlike `scan/walk.rs`, which counts
/// any entry with that name as a nested project. Whether a symlink's target exists must not
/// change which config is found, or the exit status would reveal paths outside the root.
///
/// A symlink must lead to a file really named `supdoc.config.toml`, the rule `extends` follows
/// too. It is checked before anything is read, so a hostile checkout can't make supdoc print
/// another file's lines by linking the config to it. `--config` skips this: the caller chose that
/// file (A-Q6).
///
/// # Errors
///
/// `SymlinkNotConfig` when the file found is a symlink that doesn't lead to a
/// `supdoc.config.toml` file: one dangling, looping, pointing at a directory or at a file with
/// another name are all alike.
pub(super) fn discover(start: &AbsolutePath) -> Result<Option<AbsolutePath>, ConfigError> {
    let mut dir = Some(start.clone());
    while let Some(current) = dir {
        if let Some(candidate) = config_in(&current) {
            match fs::symlink_metadata(candidate.as_path()) {
                Ok(entry) if entry.file_type().is_symlink() => {
                    return if leads_to_config(&candidate) {
                        Ok(Some(candidate))
                    } else {
                        Err(ConfigError::SymlinkNotConfig { path: candidate })
                    };
                }
                Ok(entry) if entry.is_file() => return Ok(Some(candidate)),
                _ => {}
            }
        }
        dir = current.parent();
    }
    Ok(None)
}

/// Whether `path`, once every symlink along it is followed, is a file named
/// `supdoc.config.toml`.
///
/// Every failure to follow it counts as no, so a dangling link, a loop and a link to a directory
/// all get the same answer as a link to a file with another name.
///
/// Effect: reads symlinks along `path` and the metadata of where it leads.
fn leads_to_config(path: &AbsolutePath) -> bool {
    fs::canonicalize(path.as_path()).is_ok_and(|real| {
        real.file_name()
            .is_some_and(|name| name == CONFIG_FILE_NAME)
            && fs::metadata(&real).is_ok_and(|metadata| metadata.is_file())
    })
}

/// Whether `file`, once every symlink is followed, is a file named `supdoc.config.toml`.
///
/// Effect: reads symlinks along `file`.
///
/// # Errors
///
/// Fails when `file`'s real location can't be worked out.
fn really_config(file: &AbsolutePath) -> std::io::Result<bool> {
    let real = fs::canonicalize(file.as_path())?;
    Ok(real
        .file_name()
        .is_some_and(|name| name == CONFIG_FILE_NAME))
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
/// When the value goes through a symlink, whether the link's target exists must not change the
/// error, or the exit status would reveal paths outside the root: a link that is dangling,
/// looping, or leads to a directory or to a file with another name is refused alike, as
/// [`discover`] does for the config it finds.
///
/// # Errors
///
/// `ExtendsNotConfig` for any other name or for a symlink that doesn't lead to a config file,
/// `ExtendsMissing` or `ExtendsDirectory` when a target reached without symlinks isn't a file,
/// `Read` for other failures.
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
    if through_symlink(&directory_of(from), &resolved) {
        return if leads_to_config(&resolved) {
            Ok(resolved)
        } else {
            Err(not_config())
        };
    }
    match fs::metadata(resolved.as_path()) {
        Ok(metadata) if metadata.is_dir() => Err(directory()),
        Ok(_) => match really_config(&resolved) {
            Ok(true) => Ok(resolved),
            Ok(false) => Err(not_config()),
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

/// Whether reaching `resolved` from `base` follows a symlink: the final entry or any directory on
/// the way that isn't `base` or one of its ancestors.
///
/// `resolved` is `base` joined lexically with an `extends` value, so `base` and its ancestors are
/// where the value starts or climbs to, not entries it names. An entry that is missing or can't
/// be inspected isn't a symlink, and every entry is inspected without following it.
///
/// Effect: reads the metadata of each entry between `base` and `resolved`.
fn through_symlink(base: &AbsolutePath, resolved: &AbsolutePath) -> bool {
    resolved
        .as_path()
        .ancestors()
        .take_while(|entry| !base.as_path().starts_with(entry))
        .any(|entry| fs::symlink_metadata(entry).is_ok_and(|metadata| metadata.is_symlink()))
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
