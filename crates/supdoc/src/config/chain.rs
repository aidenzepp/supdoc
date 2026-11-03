//! Finding a config, reading its `extends` chain and merging the layers.
//!
//! `discover` and `read_chain` are the only functions here that read config files. `merge` reads
//! only to find where the root and each namespace directory really lie.
//!
//! supdoc may run on an untrusted checkout whose config is hostile, so neither the config found by
//! walking up nor `extends` reads anything but a file named `supdoc.config.toml`, and a namespace
//! may not name a directory outside the root.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read as _};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use crate::limits::{CommandLineLimits, ConfigLimits, Limits, SizeLimit};
use crate::namespace::NamespaceName;
use crate::path::{AbsolutePath, RelativePath, holds_git, is_regular_file};

use super::code_blocks;
use super::pattern::{AnchoredPatterns, PatternList};
use super::raw::RawConfig;
use super::{CONFIG_FILE_NAME, Config, ConfigError, Namespace, NamespaceEscape, config_entry};

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

/// `dir/supdoc.config.toml`.
fn config_in(dir: &AbsolutePath) -> AbsolutePath {
    dir.join_path(Path::new(CONFIG_FILE_NAME))
}

/// Walks up from `start` to the nearest directory holding a config file, stopping at the
/// top of the git work tree `start` lies in.
///
/// The top is the first directory holding a `.git` as git decides it ([`holds_git`]), the same
/// test the walk uses for where ignore rules stop. Its own config is still found, but nothing
/// above it is read, so a stray `supdoc.config.toml` above a repository can't re-root a run
/// outside it. A submodule or nested repository is a top too, and never reaches the config of the
/// repository around it. Outside any git work tree the walk climbs to the filesystem root.
///
/// A directory holds one when its entry named `supdoc.config.toml` is a regular file or a
/// symlink, decided with `symlink_metadata` so the link is never followed to decide. Any other
/// entry, such as a real directory or a FIFO, is skipped. Whether a symlink's target exists doesn't
/// change which config is found: the symlink is found, or refused with `SymlinkNotConfig`.
///
/// A symlink must lead to a file really named `supdoc.config.toml`, the rule `extends` follows
/// too. It is checked before anything is read, so a hostile checkout can't make supdoc print
/// another file's lines by linking the config to it. `--config` skips this: the caller chose that
/// file.
///
/// # Errors
///
/// `SymlinkNotConfig` when the file found is a symlink that doesn't lead to a
/// `supdoc.config.toml` file: one dangling, looping, pointing at a directory or at a file with
/// another name are all alike.
pub(super) fn discover(start: &AbsolutePath) -> Result<Option<AbsolutePath>, ConfigError> {
    let mut dir = Some(start.clone());
    while let Some(current) = dir {
        let candidate = config_in(&current);
        if let Some(entry) = config_entry(current.as_path()) {
            return if !entry.file_type().is_symlink() || leads_to_config(&candidate) {
                Ok(Some(candidate))
            } else {
                Err(ConfigError::SymlinkNotConfig { path: candidate })
            };
        }
        if holds_git(current.as_path()) {
            break;
        }
        dir = current.parent();
    }
    Ok(None)
}

/// Whether `path`, once every symlink along it is followed, is a regular file named
/// `supdoc.config.toml`, the one rule discovery and both `extends` branches share.
///
/// Every failure to follow it counts as no, so a dangling link, a loop, a directory and a FIFO
/// all get the same answer as a link to a file with another name. A FIFO is refused before it is
/// opened, so reading it can't block.
///
/// Effect: reads symlinks along `path` and the metadata of where it leads.
fn leads_to_config(path: &AbsolutePath) -> bool {
    fs::canonicalize(path.as_path()).is_ok_and(|real| {
        real.file_name()
            .is_some_and(|name| name == CONFIG_FILE_NAME)
            && is_regular_file(&real).unwrap_or(false)
    })
}

/// Reads `file` and every config it extends, child first, refusing any larger than `limit`
/// (`max-config-size`) before it is parsed.
///
/// `file` must exist: a missing start file is [`ConfigError::NotFound`].
///
/// # Errors
///
/// Returns the first [`ConfigError`] found while reading, parsing or following `extends`.
pub(super) fn read_chain(file: &AbsolutePath, limit: SizeLimit) -> Result<Vec<Layer>, ConfigError> {
    let root = directory_of(file);
    let mut layers = Vec::new();
    // Each visited file with the name the filesystem knows it by, so a loop through a symlink
    // is still caught.
    let mut visited: Vec<(PathBuf, AbsolutePath)> = Vec::new();
    let mut next = Some(file.clone());
    while let Some(current) = next.take() {
        let text = read_text(&current, layers.is_empty(), limit)?;
        let raw: RawConfig = toml::from_str(&text).map_err(|error| ConfigError::Parse {
            path: current.clone(),
            error,
        })?;
        visited.push((identity(&current), current.clone()));
        if let Some(extends) = &raw.extends {
            let target = resolve_extends(&root, &current, extends)?;
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

/// Reads a config file's text, at most `limit` bytes of it. The start file being absent is
/// `NotFound`; any other file was already confirmed to exist.
///
/// The size is checked on the open handle, and the read stops one byte past the limit on that same
/// handle, so a file that grows after the check is still refused.
///
/// # Errors
///
/// `NotFound` for a missing start file, `TooLarge` for a file over `limit`, otherwise `Read` with
/// the operating system's reason, `InvalidData` for text that isn't UTF-8.
pub(super) fn read_text(
    file: &AbsolutePath,
    is_start: bool,
    limit: SizeLimit,
) -> Result<String, ConfigError> {
    let failed = |error: io::Error| {
        if is_start && error.kind() == ErrorKind::NotFound {
            ConfigError::NotFound { path: file.clone() }
        } else {
            ConfigError::Read {
                path: file.clone(),
                error,
            }
        }
    };
    let too_large = |size, limit| ConfigError::TooLarge {
        path: file.clone(),
        size,
        limit,
    };
    let handle = File::open(file.as_path()).map_err(failed)?;
    let mut bytes = Vec::new();
    match limit.get() {
        Some(limit) => {
            let size = handle.metadata().map_err(failed)?.len();
            if size > limit {
                return Err(too_large(size, limit));
            }
            let _read = handle
                .take(limit.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(failed)?;
            let held = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if held > limit {
                return Err(too_large(held, limit));
            }
        }
        None => {
            let _read = (&handle).read_to_end(&mut bytes).map_err(failed)?;
        }
    }
    String::from_utf8(bytes).map_err(|_| {
        failed(io::Error::new(
            ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        ))
    })
}

/// Resolves an `extends` value from the file that wrote it, checking it names a config file.
///
/// The value must be named `supdoc.config.toml`, and so must the file it really is once symlinks
/// are followed. Both are checked before anything is read, so a hostile `extends` can never make
/// supdoc print another file's lines in a parse error.
///
/// A value that leaves the root, or goes through a symlink, either leads to a
/// `supdoc.config.toml` file or is refused alike, whatever is really there: see [`plain`] for
/// why. Only a value that [`plain`] accepts is told apart as missing, a directory or unreadable.
///
/// # Errors
///
/// `ExtendsNotConfig` for any other name, or for a value [`plain`] refuses that doesn't lead to a
/// config file; `ExtendsDirectory` for a value ending in `.` or `..`, or naming a [`plain`]
/// directory (never suggesting the file that wrote it); and for a plain value, `ExtendsMissing`
/// when nothing is there, `ExtendsDirectory` for a directory, `ExtendsNotConfig` for any other
/// entry that isn't a regular file (a FIFO is refused before it is opened), `Read` for other
/// failures.
fn resolve_extends(
    root: &AbsolutePath,
    from: &AbsolutePath,
    extends: &toml::Spanned<RelativePath>,
) -> Result<AbsolutePath, ConfigError> {
    let value = extends.get_ref();
    let resolved = directory_of(from).join(value);
    let directory = || ConfigError::ExtendsDirectory {
        file: from.clone(),
        extends: value.clone(),
        span: extends.span(),
        suggestion: config_inside(root, value, &resolved).filter(|_| config_in(&resolved) != *from),
    };
    let not_config = || ConfigError::ExtendsNotConfig {
        file: from.clone(),
        extends: value.clone(),
        span: extends.span(),
    };
    match value.as_path().components().next_back() {
        None | Some(Component::CurDir | Component::ParentDir) => return Err(directory()),
        Some(Component::Normal(name)) if name == CONFIG_FILE_NAME => {}
        Some(_) if plain(root, &resolved) && resolved.as_path().is_dir() => {
            return Err(directory());
        }
        Some(_) => return Err(not_config()),
    }
    if !plain(root, &resolved) {
        return if leads_to_config(&resolved) {
            Ok(resolved)
        } else {
            Err(not_config())
        };
    }
    match fs::metadata(resolved.as_path()) {
        Ok(metadata) if metadata.is_dir() => Err(directory()),
        Ok(_) if leads_to_config(&resolved) => Ok(resolved),
        Ok(_) => Err(not_config()),
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

/// Whether `path` is provably the checkout's own: it lies beneath `root`, compared as text, and
/// no entry from `root` down to `path`, `path` included, is a symlink.
///
/// Telling missing from a directory from unreadable is a specific answer, given only for paths
/// this function accepts. Everything else gets one outcome decided by [`leads_to_config`]
/// alone, which keeps the rule simple and uniform. It is not a promise to hide whether a path
/// outside the root exists: an out-and-back symlink can still change which error appears.
///
/// The test fails closed. Text that leaves `root` is refused without looking: a case variant, a
/// firmlink or a link outside that leads back into the root is refused too, which costs a
/// specific error and nothing more. Within the root, each entry is inspected without following
/// it, from `root` downward, stopping at the first symlink, so every answer is a fact about an
/// entry the checkout made. An entry that is missing or can't be inspected isn't a symlink.
///
/// The accepted cost: a value that leaves the root, such as a monorepo's
/// `../shared/supdoc.config.toml`, still loads when a real config is there, but when it is
/// missing, a directory or unreadable it reports `config-extends-not-config`, and a value ending
/// in `..` that leaves the root gets no suggestion.
///
/// Effect: reads the metadata of entries beneath `root` on the way to `path`.
fn plain(root: &AbsolutePath, path: &AbsolutePath) -> bool {
    let Some(below) = path.strip_prefix(root) else {
        return false;
    };
    if below.as_os_str().is_empty() {
        return false;
    }
    let mut entry = root.as_path().to_path_buf();
    below.components().all(|component| {
        entry.push(component);
        !fs::symlink_metadata(&entry).is_ok_and(|metadata| metadata.is_symlink())
    })
}

/// `extends` pointing at the config file inside `directory`, if there is a [`plain`] one.
///
/// The file is inspected without following it, so a link there suggests nothing whatever it
/// leads to.
fn config_inside(
    root: &AbsolutePath,
    extends: &RelativePath,
    directory: &AbsolutePath,
) -> Option<RelativePath> {
    let candidate = config_in(directory);
    let is_plain_file = plain(root, &candidate)
        && fs::symlink_metadata(candidate.as_path()).is_ok_and(|metadata| metadata.is_file());
    if !is_plain_file {
        return None;
    }
    let suggested = extends.as_path().join(CONFIG_FILE_NAME);
    RelativePath::try_from(suggested.to_string_lossy().into_owned()).ok()
}

/// Combines the layers, child first, into one config rooted at `root`.
///
/// Each key comes from the nearest layer that sets it, including each key under `[features]`.
/// Namespaces merge by name, nearer wins, and each winner is confined to the root as it is kept.
/// Every path is anchored at the directory of the layer that wrote it.
///
/// Effect: reads file metadata and symlinks along the root and each namespace directory.
///
/// # Errors
///
/// `NamespaceOutsideRoot` for the first namespace, by name, whose directory lies outside the root.
pub(super) fn merge(root: AbsolutePath, layers: Vec<Layer>) -> Result<Config, ConfigError> {
    let mut files = Vec::with_capacity(layers.len());
    let mut include = None;
    let mut exclude = None;
    let mut written = BTreeMap::new();
    let mut line_endings = None;
    let mut code_blocks = None;
    let mut limits = ConfigLimits::default();
    for Layer { file, raw } in layers {
        let base = directory_of(&file);
        for (name, relative) in raw.namespaces {
            if let Entry::Vacant(slot) = written.entry(name) {
                let span = relative.span();
                let dir = base.join(relative.get_ref());
                let _inserted = slot.insert((dir, file.clone(), span));
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
        let (hash, raw_code_blocks) = raw.features.map_or((None, None), |features| {
            (features.hash, features.code_blocks)
        });
        if line_endings.is_none() {
            line_endings = hash
                .and_then(|hash| hash.line_endings)
                .map(|value| (value, file.clone()));
        }
        code_blocks = code_blocks::merge(code_blocks, raw_code_blocks, &file, &base);
        if let Some(raw_limits) = raw.limits {
            nearest(&mut limits.parse_timeout, raw_limits.parse_timeout, &file);
            nearest(
                &mut limits.code_block_timeout,
                raw_limits.code_block_timeout,
                &file,
            );
            nearest(&mut limits.max_lock_size, raw_limits.max_lock_size, &file);
            nearest(
                &mut limits.max_ignore_size,
                raw_limits.max_ignore_size,
                &file,
            );
        }
        files.push(file);
    }
    let real_root = super::real_root(&root);
    let namespaces = written
        .into_iter()
        .map(|(name, (dir, defined_in, span))| {
            let namespace = confined(&root, &real_root, &name, dir, (defined_in, span))?;
            Ok((name, namespace))
        })
        .collect::<Result<_, ConfigError>>()?;
    let include = include
        .unwrap_or_else(|| AnchoredPatterns::built_in(root.clone(), PatternList::everything()));
    let code_blocks = code_blocks.map(|written| written.resolve(&include, exclude.as_ref()));
    Ok(Config {
        root,
        real_root,
        files,
        include,
        exclude,
        namespaces,
        line_endings,
        code_blocks,
        in_effect: Limits::resolve(&CommandLineLimits::default(), &limits),
        limits,
    })
}

/// Fills `slot` with `value` and the `file` that wrote it, unless a nearer file already did.
fn nearest<T>(slot: &mut Option<(T, AbsolutePath)>, value: Option<T>, file: &AbsolutePath) {
    if slot.is_none() {
        *slot = value.map(|value| (value, file.clone()));
    }
}

/// The namespace `name`, once its directory is found inside the root after symlinks are followed.
///
/// A missing directory is still allowed: where it would be is what counts.
///
/// Effect: reads file metadata and symlinks along `dir`.
///
/// # Errors
///
/// `NamespaceOutsideRoot` when `dir` lies outside `real_root`, or where it lies can't be worked out.
/// `written` is the config that defined the namespace and the byte range of its value there.
fn confined(
    root: &AbsolutePath,
    real_root: &Path,
    name: &NamespaceName,
    dir: AbsolutePath,
    written: (AbsolutePath, Range<usize>),
) -> Result<Namespace, ConfigError> {
    let (defined_in, span) = written;
    let real = super::confine::real_location(dir.as_path());
    let escape = match real {
        Some(real) if real.starts_with(real_root) => return Ok(Namespace { dir, defined_in }),
        // Written outside the root: that is the problem, whatever lies beyond it.
        _ if dir.strip_prefix(root).is_none() => NamespaceEscape::AsWritten,
        None => NamespaceEscape::Unresolvable,
        // Written inside the root, so only a symlink along it can lead out.
        Some(_) => NamespaceEscape::ThroughSymlink,
    };
    Err(ConfigError::NamespaceOutsideRoot {
        file: defined_in,
        name: name.clone(),
        directory: dir,
        span,
        root: root.clone(),
        escape,
    })
}
