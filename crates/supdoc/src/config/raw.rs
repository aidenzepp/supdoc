//! The serde shape of one config file, exactly as written.

use std::collections::BTreeMap;

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
}
