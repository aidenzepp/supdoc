//! The serde shape of one config file, exactly as written.

use std::collections::BTreeMap;

use crate::hash::LineEndings;
use crate::limits::{SizeLimit, TimeLimit};
use crate::namespace::NamespaceName;
use crate::path::RelativePath;

use super::code_blocks::RawCodeBlocks;
use super::pattern::PatternList;

/// One `supdoc.config.toml` before it is merged with the files it extends.
///
/// Every field is a parsed type, so a successful `toml::from_str` means the contents are valid.
/// The one fact left to check is what `extends` points at, which needs the filesystem.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawConfig {
    /// Spanned so later errors (missing target, loop) can point at the exact text.
    pub(crate) extends: Option<toml::Spanned<RelativePath>>,
    /// `None` inherits from the parent; `Some(empty)` deliberately matches nothing.
    pub(crate) include: Option<PatternList>,
    /// `None` inherits from the parent; `Some(empty)` deliberately excludes nothing.
    pub(crate) exclude: Option<PatternList>,
    /// Short names for directories, relative to the file that wrote them. Spanned so an error
    /// about a directory can point at its value.
    #[serde(default)]
    pub(crate) namespaces: BTreeMap<NamespaceName, toml::Spanned<RelativePath>>,
    /// `None` inherits every toggle from the parent.
    pub(crate) features: Option<RawFeatures>,
    /// `None` inherits every ceiling from the parent.
    pub(crate) limits: Option<RawLimits>,
}

/// The `[limits]` table. `max-config-size` is not a key: it must apply before a file is parsed.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct RawLimits {
    /// `None` inherits the parent's setting.
    pub(crate) parse_timeout: Option<TimeLimit>,
    /// `None` inherits the parent's setting.
    pub(crate) code_block_timeout: Option<TimeLimit>,
    /// `None` inherits the parent's setting.
    pub(crate) code_block_build_timeout: Option<TimeLimit>,
    /// `None` inherits the parent's setting.
    pub(crate) max_lock_size: Option<SizeLimit>,
    /// `None` inherits the parent's setting.
    pub(crate) max_ignore_size: Option<SizeLimit>,
}

/// The `[features]` table: one sub-table per area, named in kebab-case.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct RawFeatures {
    /// `None` inherits the hash toggles from the parent.
    pub(crate) hash: Option<RawHashFeatures>,
    /// `Some` turns code-block checking on, here and in every config that extends this one.
    pub(crate) code_blocks: Option<RawCodeBlocks>,
}

/// The `[features.hash]` table.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawHashFeatures {
    /// `None` inherits the parent's setting.
    #[serde(rename = "line-endings")]
    pub(crate) line_endings: Option<LineEndings>,
}
