//! Loading `supdoc.config.toml` and its `extends` chain into a [`Config`] that cannot fail later.
//!
//! Every check happens while the TOML is read, so a loaded [`Config`] holds only valid values.

mod chain;
pub(crate) mod code_blocks;
pub(crate) mod confine;
mod error;
mod pattern;
mod raw;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::Path;

use self::code_blocks::CodeBlocks;
use self::confine::Boundary;
use crate::hash::{HashSettings, LineEndings};
use crate::limits::{CommandLineLimits, ConfigLimits, Limits, Raises, SizeLimit};
use crate::namespace::NamespaceName;
use crate::path::AbsolutePath;

use pattern::{AnchoredPatterns, PatternList};

pub(crate) use error::size_text;
pub(crate) use error::{ConfigError, NamespaceEscape};
pub(crate) use pattern::PatternDefinition;

/// The file name supdoc looks for in each directory.
pub(crate) const CONFIG_FILE_NAME: &str = "supdoc.config.toml";

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

/// A namespace's directory and the config file that defined it.
///
/// Constructed only while the chain is merged, after its directory is found inside the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Namespace {
    /// The directory, resolved from the config that wrote it. It may not exist.
    dir: AbsolutePath,
    /// The nearest config file in the chain that defines this namespace.
    defined_in: AbsolutePath,
}

impl Namespace {
    /// The directory the namespace names.
    #[must_use]
    pub(crate) fn dir(&self) -> &AbsolutePath {
        &self.dir
    }

    /// The config file that defined the namespace.
    #[must_use]
    pub(crate) fn defined_in(&self) -> &AbsolutePath {
        &self.defined_in
    }
}

/// A fully resolved config. Constructed only by [`Config::load`] and [`Config::load_file`].
#[derive(Debug, Clone)]
pub(crate) struct Config {
    /// The directory of the config that was found, not of anything it extends.
    root: AbsolutePath,
    /// `root` with every symlink followed: the boundary supdoc reads and writes inside.
    boundary: Boundary,
    /// Config files in the chain, child first. Empty when running on defaults.
    files: Vec<AbsolutePath>,
    /// Files to read links from.
    include: AnchoredPatterns,
    /// Files removed from `include`, if any key set it.
    exclude: Option<AnchoredPatterns>,
    /// Every namespace, each confined to the root when the chain was merged.
    namespaces: BTreeMap<NamespaceName, Namespace>,
    /// The nearest `[features.hash] line-endings` and the file that set it, if any file did.
    line_endings: Option<(LineEndings, AbsolutePath)>,
    /// The resolved `[features.code-blocks]` settings, or `None` when no file in the chain writes
    /// the table and the feature is off.
    code_blocks: Option<CodeBlocks>,
    /// Each `[limits]` key from the nearest config that sets it, with that file.
    limits: ConfigLimits,
    /// Every ceiling in effect: `limits` over the defaults, under the command line once
    /// [`Config::with_command_line`] has applied it.
    in_effect: Limits,
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
    pub(crate) fn load(start: &AbsolutePath) -> Result<Self, ConfigError> {
        Self::load_under(start, &CommandLineLimits::default())
    }

    /// [`Config::load`] with the ceilings `given` on the command line in effect, including
    /// `max-config-size` while each config file is read. A config raise of a ceiling is held back
    /// until [`Config::with_command_line`] honors it for a trusted root.
    ///
    /// Reads the filesystem.
    ///
    /// # Errors
    ///
    /// As [`Config::load`], and [`ConfigError::TooLarge`] for a config file in the chain larger
    /// than `max-config-size`.
    pub(crate) fn load_under(
        start: &AbsolutePath,
        given: &CommandLineLimits,
    ) -> Result<Self, ConfigError> {
        match chain::discover(start)? {
            Some(file) => Self::load_file_under(&file, given),
            None => Ok(Self::defaults(start).with_command_line(given, Raises::Held)),
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
    #[cfg(test)]
    pub(crate) fn load_file(file: &AbsolutePath) -> Result<Self, ConfigError> {
        Self::load_file_under(file, &CommandLineLimits::default())
    }

    /// [`Config::load_file`] with the ceilings `given` on the command line in effect, including
    /// `max-config-size` while each config file is read. A config raise of a ceiling is held back,
    /// as in [`Config::load_under`].
    ///
    /// Reads the filesystem.
    ///
    /// # Errors
    ///
    /// As [`Config::load_file`], and [`ConfigError::TooLarge`] for a config file in the chain
    /// larger than `max-config-size`.
    pub(crate) fn load_file_under(
        file: &AbsolutePath,
        given: &CommandLineLimits,
    ) -> Result<Self, ConfigError> {
        let layers = chain::read_chain(file, config_size_limit(given))?;
        Ok(chain::merge(chain::directory_of(file), layers)?.with_command_line(given, Raises::Held))
    }

    /// The config used when no file is found: scan everything beneath `root`.
    fn defaults(root: &AbsolutePath) -> Self {
        Self {
            root: root.clone(),
            boundary: Boundary::resolve(root),
            files: Vec::new(),
            include: AnchoredPatterns::built_in(root.clone(), PatternList::everything()),
            exclude: None,
            namespaces: BTreeMap::new(),
            line_endings: None,
            code_blocks: None,
            limits: ConfigLimits::default(),
            in_effect: Limits::resolve(&CommandLineLimits::default(), &ConfigLimits::default()),
        }
    }

    /// The project root: the directory of the config that was found. `supdoc.lock` sits here.
    #[must_use]
    pub(crate) fn root(&self) -> &AbsolutePath {
        &self.root
    }

    /// Whether `path` lies inside the project root once every symlink along it is followed.
    ///
    /// A dangling symlink counts where it points, and a path whose real location can't be worked
    /// out counts as outside, so nothing outside the root is ever read through it.
    ///
    /// Effect: reads file metadata and symlinks along `path`.
    #[must_use]
    pub(crate) fn contains(&self, path: &Path) -> bool {
        self.boundary.contains(path)
    }

    /// The project root as a write boundary: every write inside the project passes its
    /// [`Boundary::admit`] first.
    #[must_use]
    pub(crate) fn boundary(&self) -> &Boundary {
        &self.boundary
    }

    /// The config files in the chain, child first. Empty when running on defaults.
    #[must_use]
    pub(crate) fn files(&self) -> &[AbsolutePath] {
        &self.files
    }

    /// Whether `path` is included and not excluded.
    #[must_use]
    pub(crate) fn scans(&self, path: &AbsolutePath) -> bool {
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
    pub(crate) fn excluded_by(&self, path: &AbsolutePath) -> Option<&str> {
        self.exclude.as_ref()?.first_match(path)
    }

    /// The directory a namespace names, or `None` if the config doesn't define it.
    ///
    /// The directory may not exist: that is checked when a link through it is read.
    #[must_use]
    pub(crate) fn namespace(&self, name: &NamespaceName) -> Option<&AbsolutePath> {
        self.namespaces.get(name).map(Namespace::dir)
    }

    /// Every namespace, by name.
    #[must_use]
    pub(crate) fn namespaces(&self) -> &BTreeMap<NamespaceName, Namespace> {
        &self.namespaces
    }

    /// The `include` patterns as written and the file that set them, or `None` when no config
    /// file sets `include` and the default (everything) applies.
    #[must_use]
    pub(crate) fn include_definition(&self) -> Option<PatternDefinition<'_>> {
        self.include.definition()
    }

    /// The `exclude` patterns as written and the file that set them, or `None` when no config
    /// file sets `exclude`.
    #[must_use]
    pub(crate) fn exclude_definition(&self) -> Option<PatternDefinition<'_>> {
        self.exclude.as_ref()?.definition()
    }

    /// The config file that defined the namespace `name`, or `None` if the config doesn't define
    /// it. When several files in the chain define it, this is the nearest one.
    #[must_use]
    pub(crate) fn namespace_defined_in(&self, name: &NamespaceName) -> Option<&AbsolutePath> {
        self.namespaces.get(name).map(Namespace::defined_in)
    }

    /// The resolved hash settings: each key from the nearest config that sets it, else the
    /// default.
    #[must_use]
    pub(crate) fn hash_settings(&self) -> HashSettings {
        HashSettings {
            line_endings: self
                .line_endings
                .as_ref()
                .map(|(value, _)| *value)
                .unwrap_or_default(),
        }
    }

    /// The resolved `[features.code-blocks]` settings, or `None` when the feature is off.
    #[must_use]
    pub(crate) const fn code_blocks(&self) -> Option<&CodeBlocks> {
        self.code_blocks.as_ref()
    }

    /// The config file that set `line-endings`, or `None` when the default applies.
    #[must_use]
    pub(crate) fn line_endings_defined_in(&self) -> Option<&AbsolutePath> {
        self.line_endings.as_ref().map(|(_, file)| file)
    }

    /// Each `[limits]` key from the nearest config that sets it, and that file.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn limits(&self) -> &ConfigLimits {
        &self.limits
    }

    /// This config with the ceilings `given` on the command line put in effect over its own, by
    /// flag beats variable beats config beats default. A config value above its default takes
    /// effect only when `raises` is [`Raises::Honored`], which is the root's trust to grant.
    #[must_use]
    pub(crate) fn with_command_line(mut self, given: &CommandLineLimits, raises: Raises) -> Self {
        self.in_effect = Limits::resolve_with(given, &self.limits, raises);
        self
    }

    /// Every ceiling in effect, and where each came from. Every reader takes its ceiling from
    /// here, so one run applies one value.
    #[must_use]
    pub(crate) fn limits_in_effect(&self) -> &Limits {
        &self.in_effect
    }
}

/// The `max-config-size` in effect for `given`: no config file sets it, so the command line and
/// the default decide it alone.
pub(crate) fn config_size_limit(given: &CommandLineLimits) -> SizeLimit {
    Limits::resolve(given, &ConfigLimits::default())
        .max_config_size
        .value
}

/// The text of the config file `file`, refused as [`ConfigError::TooLarge`] past `limit`, the one
/// capped read every config file goes through.
///
/// Effect: reads `file`.
///
/// # Errors
///
/// [`ConfigError::TooLarge`] for a file over `limit`, otherwise [`ConfigError::Read`].
pub(crate) fn read_config_text(
    file: &AbsolutePath,
    limit: SizeLimit,
) -> Result<String, ConfigError> {
    chain::read_text(file, false, limit)
}
