//! Loading `supdoc.config.toml` and its `extends` chain into a [`Config`] that cannot fail later.
//!
//! Every check happens while the TOML is read, so a loaded [`Config`] holds only valid values.

mod chain;
mod error;
mod pattern;
mod raw;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use crate::namespace::NamespaceName;
use crate::path::AbsolutePath;

use pattern::{AnchoredPatterns, PatternList};

pub use error::ConfigError;

/// The file name supdoc looks for in each directory.
pub const CONFIG_FILE_NAME: &str = "supdoc.config.toml";

/// A fully resolved config. Constructed only by [`Config::load`] and [`Config::load_file`].
#[derive(Debug, Clone)]
pub struct Config {
    /// The directory of the config that was found, not of anything it extends.
    root: AbsolutePath,
    /// Config files in the chain, child first. Empty when running on defaults.
    files: Vec<AbsolutePath>,
    /// Files to read links from.
    include: AnchoredPatterns,
    /// Files removed from `include`, if any key set it.
    exclude: Option<AnchoredPatterns>,
    /// Namespace directories, each resolved from the config that wrote it.
    namespaces: BTreeMap<NamespaceName, AbsolutePath>,
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
        match chain::discover(start) {
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
        Ok(chain::merge(chain::directory_of(file), layers))
    }

    /// The config used when no file is found: scan everything beneath `root`.
    fn defaults(root: &AbsolutePath) -> Self {
        Self {
            root: root.clone(),
            files: Vec::new(),
            include: AnchoredPatterns::new(root.clone(), PatternList::everything()),
            exclude: None,
            namespaces: BTreeMap::new(),
        }
    }

    /// The project root: the directory of the config that was found. `supdoc.lock` sits here.
    #[must_use]
    pub fn root(&self) -> &AbsolutePath {
        &self.root
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
}
