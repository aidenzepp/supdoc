//! Loading `supdoc.config.toml` and its `extends` chain into a [`Config`] that cannot fail later.
//!
//! Every check happens while the TOML is read, so a loaded [`Config`] holds only valid values.

mod chain;
pub(crate) mod confine;
mod error;
mod pattern;
mod raw;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::hash::{HashSettings, LineEndings};
use crate::namespace::NamespaceName;
use crate::path::AbsolutePath;

use pattern::{AnchoredPatterns, PatternList};

pub use error::ConfigError;
pub use pattern::PatternDefinition;

/// The file name supdoc looks for in each directory.
pub const CONFIG_FILE_NAME: &str = "supdoc.config.toml";

/// The metadata of `dir`'s entry named `supdoc.config.toml`, if that entry counts as a config.
///
/// An entry counts only when a no-follow check (`symlink_metadata`) shows a regular file or a
/// symlink. A directory, a FIFO or any other kind of entry with that name does not count, and
/// no symlink is followed to decide.
///
/// Effect: reads the metadata of one directory entry.
pub(crate) fn config_entry(dir: &Path) -> Option<std::fs::Metadata> {
    std::fs::symlink_metadata(dir.join(CONFIG_FILE_NAME))
        .ok()
        .filter(|entry| entry.is_file() || entry.is_symlink())
}

/// The config file [`Config::load`] would load when started from `start`, or `None` when there
/// is none, without reading it.
///
/// Effect: reads directory entries and symlinks upward from `start`.
///
/// # Errors
///
/// [`ConfigError::SymlinkNotConfig`] when the nearest entry is a symlink that doesn't lead to a
/// file named `supdoc.config.toml`.
pub(crate) fn discover_config(start: &AbsolutePath) -> Result<Option<AbsolutePath>, ConfigError> {
    chain::discover(start)
}

/// A fully resolved config. Constructed only by [`Config::load`] and [`Config::load_file`].
#[derive(Debug, Clone)]
pub struct Config {
    /// The directory of the config that was found, not of anything it extends.
    root: AbsolutePath,
    /// `root` with every symlink followed: the boundary supdoc reads inside.
    real_root: PathBuf,
    /// Config files in the chain, child first. Empty when running on defaults.
    files: Vec<AbsolutePath>,
    /// Files to read links from.
    include: AnchoredPatterns,
    /// Files removed from `include`, if any key set it.
    exclude: Option<AnchoredPatterns>,
    /// Namespace directories, each resolved from the config that wrote it.
    namespaces: BTreeMap<NamespaceName, AbsolutePath>,
    /// The config file that defined each namespace in `namespaces`, under the same names.
    namespace_files: BTreeMap<NamespaceName, AbsolutePath>,
    /// The nearest `[features.hash] line-endings` and the file that set it, if any file did.
    line_endings: Option<(LineEndings, AbsolutePath)>,
}

impl Config {
    /// Searches upward from `start` for `supdoc.config.toml`, falling back to defaults rooted at
    /// `start` when there is none.
    ///
    /// Reads the filesystem.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] if the config, or a file it extends, can't be read, parsed or
    /// followed.
    pub fn load(start: &AbsolutePath) -> Result<Self, ConfigError> {
        match chain::discover(start)? {
            Some(file) => Self::load_file(&file),
            None => Ok(Self::defaults(start)),
        }
    }

    /// Loads exactly `file`, as the `--config` flag asks. Never falls back to defaults.
    ///
    /// Reads the filesystem.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::NotFound`] if `file` doesn't exist, and the other variants as
    /// [`Config::load`] does.
    pub fn load_file(file: &AbsolutePath) -> Result<Self, ConfigError> {
        let layers = chain::read_chain(file)?;
        let config = chain::merge(chain::directory_of(file), layers);
        chain::confine_namespaces(&config)?;
        Ok(config)
    }

    /// The config used when no file is found: scan everything beneath `root`.
    fn defaults(root: &AbsolutePath) -> Self {
        Self {
            root: root.clone(),
            real_root: real_root(root),
            files: Vec::new(),
            include: AnchoredPatterns::built_in(root.clone(), PatternList::everything()),
            exclude: None,
            namespaces: BTreeMap::new(),
            namespace_files: BTreeMap::new(),
            line_endings: None,
        }
    }

    /// The project root: the directory of the config that was found. `supdoc.lock` sits here.
    #[must_use]
    pub fn root(&self) -> &AbsolutePath {
        &self.root
    }

    /// Whether `path` lies inside the project root once every symlink along it is followed.
    ///
    /// A dangling symlink counts where it points, and a path whose real location can't be worked
    /// out counts as outside, so nothing outside the root is ever read through it.
    ///
    /// Effect: reads file metadata and symlinks along `path`.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        confine::real_location(path).is_some_and(|real| real.starts_with(&self.real_root))
    }

    /// The config files in the chain, child first. Empty when running on defaults.
    #[must_use]
    pub fn files(&self) -> &[AbsolutePath] {
        &self.files
    }

    /// Whether `path` is included and not excluded.
    #[must_use]
    pub fn scans(&self, path: &AbsolutePath) -> bool {
        self.include.matches(path)
            && !self
                .exclude
                .as_ref()
                .is_some_and(|exclude| exclude.matches(path))
    }

    /// The `exclude` pattern, as written, that excludes `path` (a file or a directory), or `None`
    /// when no pattern does. Patterns match everything beneath what they name, so a path inside
    /// an excluded directory is excluded by that directory's pattern.
    #[must_use]
    pub fn excluded_by(&self, path: &AbsolutePath) -> Option<&str> {
        self.exclude.as_ref()?.first_match(path)
    }

    /// The directory a namespace names, or `None` if the config doesn't define it.
    ///
    /// The directory may not exist: that is checked when a link through it is read (01 D7).
    #[must_use]
    pub fn namespace(&self, name: &NamespaceName) -> Option<&AbsolutePath> {
        self.namespaces.get(name)
    }

    /// Every namespace, by name.
    #[must_use]
    pub fn namespaces(&self) -> &BTreeMap<NamespaceName, AbsolutePath> {
        &self.namespaces
    }

    /// The `include` patterns as written and the file that set them, or `None` when no config
    /// file sets `include` and the default (everything) applies.
    #[must_use]
    pub fn include_definition(&self) -> Option<PatternDefinition<'_>> {
        self.include.definition()
    }

    /// The `exclude` patterns as written and the file that set them, or `None` when no config
    /// file sets `exclude`.
    #[must_use]
    pub fn exclude_definition(&self) -> Option<PatternDefinition<'_>> {
        self.exclude.as_ref()?.definition()
    }

    /// The config file that defined the namespace `name`, or `None` if the config doesn't define
    /// it. When several files in the chain define it, this is the nearest one.
    #[must_use]
    pub fn namespace_defined_in(&self, name: &NamespaceName) -> Option<&AbsolutePath> {
        self.namespace_files.get(name)
    }

    /// The resolved hash settings: each key from the nearest config that sets it, else the
    /// default.
    #[must_use]
    pub fn hash_settings(&self) -> HashSettings {
        HashSettings {
            line_endings: self
                .line_endings
                .as_ref()
                .map(|(value, _)| *value)
                .unwrap_or_default(),
        }
    }

    /// The config file that set `line-endings`, or `None` when the default applies.
    #[must_use]
    pub fn line_endings_defined_in(&self) -> Option<&AbsolutePath> {
        self.line_endings.as_ref().map(|(_, file)| file)
    }
}

/// `root` with every symlink followed, or `root` as written when it can't be resolved.
///
/// Effect: reads the filesystem.
fn real_root(root: &AbsolutePath) -> PathBuf {
    confine::real_location(root.as_path()).unwrap_or_else(|| root.as_path().to_path_buf())
}
