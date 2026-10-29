//! The serde shape of one config file, exactly as written.

use std::collections::BTreeMap;

use crate::hash::LineEndings;
use crate::namespace::NamespaceName;
use crate::path::RelativePath;

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
    /// Short names for directories, relative to the file that wrote them.
    #[serde(default)]
    pub(crate) namespaces: BTreeMap<NamespaceName, RelativePath>,
    /// `None` inherits every toggle from the parent.
    pub(crate) features: Option<RawFeatures>,
}

/// The `[features]` table: one sub-table per area (01 D10).
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawFeatures {
    /// `None` inherits the hash toggles from the parent.
    pub(crate) hash: Option<RawHashFeatures>,
}

/// The `[features.hash]` table.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawHashFeatures {
    /// `None` inherits the parent's setting.
    #[serde(rename = "line-endings")]
    pub(crate) line_endings: Option<LineEndings>,
}
