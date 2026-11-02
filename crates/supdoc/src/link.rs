//! Links from documentation to code: where a link is written, where it points, and the identity
//! the lockfile records for it.
//!
//! Every path here is project-relative and `/`-separated whatever the operating system, because
//! these values are written into `supdoc.lock`, which is committed and shared.

use std::fmt;
use std::ops::Range;
use std::path::{Component, Path};
use std::str::FromStr;

use crate::namespace::{NamespaceName, NamespaceNameError};

/// Parsing a Markdown link destination into a link target.
pub(crate) mod destination;

/// A file that contains links (a doc **or a code file**), relative to the project root.
///
/// `/`-separated, not empty, with no `.`, `..` or empty segments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct SourcePath(String);

impl SourcePath {
    /// Builds a source path from a relative filesystem path, joining its components with `/`.
    ///
    /// # Errors
    ///
    /// Fails if `path` is rooted, contains `..`, is empty, or has a component that isn't UTF-8.
    pub(crate) fn from_relative(path: &Path) -> Result<Self, LinkPathError> {
        let mut segments = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(segment) => match segment.to_str() {
                    Some(segment) => segments.push(segment),
                    None => {
                        return Err(LinkPathError::NotUtf8 {
                            path: path.to_string_lossy().into_owned(),
                        });
                    }
                },
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(LinkPathError::ParentSegment {
                        path: path.to_string_lossy().into_owned(),
                    });
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(LinkPathError::Absolute {
                        path: path.to_string_lossy().into_owned(),
                    });
                }
            }
        }
        Self::try_from(segments.join("/"))
    }

    /// The path as written, with `/` separators.
    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SourcePath {
    type Error = LinkPathError;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        check_link_path(&path, |path| LinkPathError::ParentSegment { path })?;
        Ok(Self(path))
    }
}

impl FromStr for SourcePath {
    type Err = LinkPathError;

    fn from_str(path: &str) -> Result<Self, Self::Err> {
        Self::try_from(path.to_owned())
    }
}

/// The path part of a [`Target`]: relative to the project root (or to a namespace's directory),
/// `/`-separated, not empty, with no `.`, `..` or empty segments. supdoc reads no target outside
/// the project root, so a target that climbs with `..` can't be represented.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TargetPath(String);

impl TargetPath {
    /// The path as written, with `/` separators.
    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TargetPath {
    type Error = LinkPathError;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        check_link_path(&path, |path| LinkPathError::TargetClimbs { path })?;
        if path.contains(':') {
            return Err(LinkPathError::Colon { path });
        }
        Ok(Self(path))
    }
}

impl FromStr for TargetPath {
    type Err = LinkPathError;

    fn from_str(path: &str) -> Result<Self, Self::Err> {
        Self::try_from(path.to_owned())
    }
}

/// Checks the shared rules for a project-relative, `/`-separated link path. A `..` segment is
/// refused with `climbs`, so sources and targets each name their own reason.
///
/// # Errors
///
/// Returns the first rule `path` breaks.
fn check_link_path(path: &str, climbs: fn(String) -> LinkPathError) -> Result<(), LinkPathError> {
    let error = |make: fn(String) -> LinkPathError| Err(make(path.to_owned()));
    if path.is_empty() {
        return Err(LinkPathError::Empty);
    }
    if path.contains('\\') {
        return error(|path| LinkPathError::Backslash { path });
    }
    if path.starts_with('/') {
        return error(|path| LinkPathError::Absolute { path });
    }
    for segment in path.split('/') {
        match segment {
            "" => return error(|path| LinkPathError::EmptySegment { path }),
            "." => return error(|path| LinkPathError::DotSegment { path }),
            ".." => return error(climbs),
            _ => {}
        }
    }
    Ok(())
}

/// Why a string is not a valid project-relative link path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum LinkPathError {
    /// The path is empty.
    #[error("the path is empty")]
    Empty,
    /// The path uses `\` instead of `/`.
    #[error("`{path}` uses `\\`; write paths with `/` on every platform")]
    Backslash {
        /// The rejected path.
        path: String,
    },
    /// The path starts at a filesystem root instead of the project root.
    #[error("`{path}` is absolute; write it relative to the project root")]
    Absolute {
        /// The rejected path.
        path: String,
    },
    /// The path has an empty segment (`a//b` or a trailing `/`).
    #[error("`{path}` has an empty segment (`//` or a trailing `/`)")]
    EmptySegment {
        /// The rejected path.
        path: String,
    },
    /// The path has a `.` segment.
    #[error("`{path}` has a `.` segment; remove it")]
    DotSegment {
        /// The rejected path.
        path: String,
    },
    /// A source path climbs out of the project root.
    #[error("`{path}` uses `..`; a source must be inside the project root")]
    ParentSegment {
        /// The rejected path.
        path: String,
    },
    /// A plain target path climbs out of the project root with `..`.
    #[error(
        "target `{path}` climbs out of the project root with `..`, and supdoc reads only files inside it"
    )]
    TargetClimbs {
        /// The rejected path.
        path: String,
    },
    /// A namespaced target path uses `..`, so it could leave its namespace's directory.
    #[error(
        "target `{namespace}:{path}` uses `..`, and supdoc reads a namespaced target only inside the `{namespace}` namespace's directory"
    )]
    NamespacedTargetClimbs {
        /// The namespace the target names, `auth`.
        namespace: String,
        /// The rejected path inside the namespace's directory.
        path: String,
    },
    /// A target path contains `:`, which the lockfile reserves for namespace prefixes.
    #[error(
        "`{path}` contains `:`; rename the file or directory, because `:` is reserved for namespace prefixes"
    )]
    Colon {
        /// The rejected path.
        path: String,
    },
    /// A filesystem path has a component that isn't UTF-8.
    #[error("`{path}` is not valid UTF-8")]
    NotUtf8 {
        /// The rejected path, with invalid bytes replaced.
        path: String,
    },
}

/// Where a link points, exactly as written in the doc.
///
/// Ordering puts plain paths before namespaced ones, then sorts by namespace and path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Deserialize)]
#[serde(try_from = "String")]
pub(crate) enum Target {
    /// `src/lib.rs`, `docs/guide.md`: relative to the project root.
    Path(TargetPath),
    /// `auth:src/lib.rs`: relative to the namespace's directory.
    Namespaced {
        /// The namespace, `auth`.
        namespace: NamespaceName,
        /// The path inside the namespace's directory, `src/lib.rs`.
        path: TargetPath,
    },
}

impl Target {
    /// The namespace, if the target is namespaced.
    #[must_use]
    pub(crate) fn namespace(&self) -> Option<&NamespaceName> {
        match self {
            Self::Path(_) => None,
            Self::Namespaced { namespace, .. } => Some(namespace),
        }
    }

    /// The path, relative to the project root or to the namespace's directory.
    #[must_use]
    pub(crate) fn path(&self) -> &TargetPath {
        match self {
            Self::Path(path) | Self::Namespaced { path, .. } => path,
        }
    }
}

impl FromStr for Target {
    type Err = TargetError;

    /// Parses `path` or `namespace:path`. Everything before the first `:` must be a namespace name.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.split_once(':') {
            None => Ok(Self::Path(raw.parse()?)),
            Some((namespace, path)) => {
                let namespace: NamespaceName = namespace.parse()?;
                let path = path.parse().map_err(|error| match error {
                    LinkPathError::TargetClimbs { path } => LinkPathError::NamespacedTargetClimbs {
                        namespace: namespace.to_string(),
                        path,
                    },
                    other => other,
                })?;
                Ok(Self::Namespaced { namespace, path })
            }
        }
    }
}

impl TryFrom<String> for Target {
    type Error = TargetError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(path) => f.write_str(path.as_str()),
            Self::Namespaced { namespace, path } => write!(f, "{namespace}:{}", path.as_str()),
        }
    }
}

/// Why a string is not a valid [`Target`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TargetError {
    /// The text before `:` is not a valid namespace name.
    #[error(transparent)]
    Namespace(#[from] NamespaceNameError),
    /// The path is not a valid project-relative path.
    #[error(transparent)]
    Path(#[from] LinkPathError),
}

/// A dotted symbol path: `add`, `Config.validate`, `getting-started.install`.
///
/// One or more segments, none empty and none containing `.`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct SymbolPath(Vec<String>);

/// One segment of a [`SymbolPath`]: non-empty and free of `.`.
///
/// Holding one proves the segment is valid, so [`SymbolPath::root`] and [`SymbolPath::child`]
/// cannot fail.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SymbolSegment(String);

impl SymbolSegment {
    /// Checks `name` as a symbol path segment.
    ///
    /// # Errors
    ///
    /// Fails if `name` is empty or contains `.`.
    pub(crate) fn new(name: String) -> Result<Self, SymbolPathError> {
        SymbolPath::from_segments(vec![name]).map(|path| Self(path.0.into_iter().collect()))
    }

    /// This segment with `-number` appended, which keeps it valid.
    #[must_use]
    pub(crate) fn numbered(&self, number: usize) -> Self {
        Self(format!("{}-{number}", self.0))
    }
}

impl SymbolPath {
    /// A one-segment path.
    #[must_use]
    pub(crate) fn root(segment: SymbolSegment) -> Self {
        Self(vec![segment.0])
    }

    /// This path extended by one more segment.
    #[must_use]
    pub(crate) fn child(&self, segment: SymbolSegment) -> Self {
        let mut segments = self.0.clone();
        segments.push(segment.0);
        Self(segments)
    }

    /// Builds a path from its segments, outermost first.
    ///
    /// # Errors
    ///
    /// Fails if there are no segments, or a segment is empty or contains `.`.
    pub(crate) fn from_segments(segments: Vec<String>) -> Result<Self, SymbolPathError> {
        let invalid = |reason| SymbolPathError {
            input: segments.join("."),
            reason,
        };
        if segments.is_empty() {
            return Err(invalid(SymbolPathError::EMPTY));
        }
        if segments.iter().any(String::is_empty) {
            return Err(invalid(SymbolPathError::EMPTY_SEGMENT));
        }
        if segments.iter().any(|segment| segment.contains('.')) {
            return Err(invalid(SymbolPathError::DOT_IN_SEGMENT));
        }
        Ok(Self(segments))
    }

    /// The segments, outermost first. Never empty.
    #[must_use]
    pub(crate) fn segments(&self) -> &[String] {
        &self.0
    }

    /// The last segment: the symbol's own name.
    #[must_use]
    pub(crate) fn last(&self) -> &str {
        // Construction guarantees at least one segment, so the fallback is never used.
        self.0.last().map_or("", String::as_str)
    }
}

impl FromStr for SymbolPath {
    type Err = SymbolPathError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.is_empty() {
            return Err(SymbolPathError {
                input: String::new(),
                reason: SymbolPathError::EMPTY,
            });
        }
        Self::from_segments(raw.split('.').map(str::to_owned).collect())
    }
}

impl TryFrom<String> for SymbolPath {
    type Error = SymbolPathError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl fmt::Display for SymbolPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("."))
    }
}

/// Why a string is not a valid [`SymbolPath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid symbol `{input}`: {reason}")]
pub(crate) struct SymbolPathError {
    /// The rejected symbol, as written.
    pub(crate) input: String,
    /// What is wrong with it, as a phrase (`a segment is empty`).
    pub(crate) reason: &'static str,
}

impl SymbolPathError {
    /// The reason for an empty symbol.
    pub(crate) const EMPTY: &'static str = "the symbol is empty";
    /// The reason for `a..b`, `.a` or `a.`.
    pub(crate) const EMPTY_SEGMENT: &'static str = "a segment is empty";
    /// The reason for a segment built with a `.` inside it.
    pub(crate) const DOT_IN_SEGMENT: &'static str = "a segment contains `.`";
}

/// Identity of one lock entry. Field order is the sort order.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, schemars::JsonSchema,
)]
pub(crate) struct LinkKey {
    /// The file the link is written in.
    pub(crate) source: SourcePath,
    /// The file the link points at.
    pub(crate) target: Target,
    /// `None` = whole file. `Option`'s ordering puts whole-file entries first. Left out of JSON
    /// when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) symbol: Option<SymbolPath>,
}

impl fmt::Display for LinkKey {
    /// `docs/guide.md → src/lib.rs#Config.validate`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} → ", self.source.as_str())?;
        write_target(f, &self.target, self.symbol.as_ref())
    }
}

impl LinkKey {
    /// The target part alone, as `path`, `path#symbol` or `ns:path#symbol`.
    #[must_use]
    pub(crate) fn target_text(&self) -> String {
        let mut text = String::new();
        // A `String` never fails to take a write, so the result carries no information.
        let _ = write_target(&mut text, &self.target, self.symbol.as_ref());
        text
    }
}

/// Writes `target`, then `#symbol` when there is one: the one rendering of a link's target.
///
/// # Errors
///
/// Fails if writing to `out` fails.
fn write_target(
    out: &mut impl fmt::Write,
    target: &Target,
    symbol: Option<&SymbolPath>,
) -> fmt::Result {
    write!(out, "{target}")?;
    if let Some(symbol) = symbol {
        write!(out, "#{symbol}")?;
    }
    Ok(())
}

/// What a link points at, without its source: `path`, `path#symbol` or `ns:path#symbol`.
///
/// Used where a user names a target on the command line (`supdoc list src/lib.rs#add`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct LinkRef {
    /// The file.
    pub(crate) target: Target,
    /// `None` = the whole file.
    pub(crate) symbol: Option<SymbolPath>,
}

impl FromStr for LinkRef {
    type Err = LinkRefError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.split_once('#') {
            None => Ok(Self {
                target: raw.parse()?,
                symbol: None,
            }),
            Some((target, symbol)) => Ok(Self {
                target: target.parse()?,
                symbol: Some(symbol.parse()?),
            }),
        }
    }
}

impl fmt::Display for LinkRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_target(f, &self.target, self.symbol.as_ref())
    }
}

/// Why a string is not a valid [`LinkRef`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum LinkRefError {
    /// The part before `#` is not a valid target.
    #[error(transparent)]
    Target(#[from] TargetError),
    /// The part after `#` is not a valid symbol path.
    #[error(transparent)]
    Symbol(#[from] SymbolPathError),
}

/// A link exactly as found in a source file, before its destination is understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawLink {
    /// The link text, for diagnostics.
    pub(crate) text: String,
    /// The destination as written, after reference-link resolution.
    pub(crate) destination: String,
    /// Where the link sits in the source file, in bytes.
    pub(crate) range: Range<usize>,
    /// Where the destination is written in the source file, in bytes, without the `<…>` that may
    /// wrap it: inside the link for an inline link, in its `[label]:` definition for a reference
    /// link. Every use of one label shares its definition's range.
    pub(crate) destination_range: Range<usize>,
}

/// Serializes a value as its `Display` text.
macro_rules! serialize_as_display {
    ($($type:ty),+) => {$(
        impl serde::Serialize for $type {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }
    )+};
}

serialize_as_display!(SourcePath, TargetPath, Target, SymbolPath, LinkRef);

/// Describes a value that serializes as its text, as a plain JSON string.
macro_rules! string_schema {
    ($($type:ty => $description:literal),+ $(,)?) => {$(
        impl schemars::JsonSchema for $type {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($type).into()
            }

            fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({ "type": "string", "description": $description })
            }
        }
    )+};
}

string_schema! {
    SourcePath => "A file path relative to the project root, with `/` separators.",
    TargetPath => "A path a link points at, with `/` separators.",
    Target => "Where a link points: `path` or `namespace:path`.",
    SymbolPath => "A dotted symbol path such as `Config.validate`.",
    LinkRef => "A link target as typed on the command line: `path`, `path#symbol` or `ns:path#symbol`.",
}

impl fmt::Display for SourcePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for TargetPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(raw: &str) -> SourcePath {
        raw.parse()
            .expect("the fixture source path should be valid")
    }

    fn target(raw: &str) -> Target {
        raw.parse().expect("the fixture target should be valid")
    }

    fn symbol(raw: &str) -> SymbolPath {
        raw.parse().expect("the fixture symbol should be valid")
    }

    #[test]
    fn target_path_rejects_a_colon() {
        assert_eq!(
            "docs/a:b.rs".parse::<TargetPath>(),
            Err(LinkPathError::Colon {
                path: "docs/a:b.rs".to_owned(),
            }),
        );
    }

    #[test]
    fn namespaced_target_round_trips_with_the_colon_rule_on_the_path_only() {
        let target: Target = "auth:src/x.rs"
            .parse()
            .expect("a namespaced target should parse");
        assert_eq!(target.to_string(), "auth:src/x.rs");
    }

    #[test]
    fn source_with_parent_segment_is_rejected() {
        assert_eq!(
            "../docs/guide.md".parse::<SourcePath>(),
            Err(LinkPathError::ParentSegment {
                path: "../docs/guide.md".to_owned()
            }),
        );
    }

    #[test]
    fn target_with_parent_segment_is_rejected_plain_or_namespaced() {
        for raw in ["../shared/x.rs", "src/../../x.rs"] {
            assert_eq!(
                raw.parse::<Target>(),
                Err(TargetError::Path(LinkPathError::TargetClimbs {
                    path: raw.to_owned()
                })),
                "target `{raw}` should not parse",
            );
        }
        assert_eq!(
            "docs:../outside/x.rs".parse::<Target>(),
            Err(TargetError::Path(LinkPathError::NamespacedTargetClimbs {
                namespace: "docs".to_owned(),
                path: "../outside/x.rs".to_owned(),
            })),
            "a namespaced target with `..` should not parse",
        );
    }

    #[test]
    fn namespaced_climbing_target_message_keeps_the_namespace() {
        let error = "auth:../web/x.rs"
            .parse::<Target>()
            .expect_err("a namespaced target with `..` should not parse");
        let message = error.to_string();
        assert!(
            message.contains("`auth:../web/x.rs`") && !message.contains("project root"),
            "the message should name the whole target and not blame the project root: {message}",
        );
    }

    #[test]
    fn backslash_paths_are_rejected() {
        let expected = LinkPathError::Backslash {
            path: r"docs\guide.md".to_owned(),
        };

        assert_eq!(
            r"docs\guide.md".parse::<SourcePath>(),
            Err(expected.clone())
        );
        assert_eq!(
            r"docs\guide.md".parse::<Target>(),
            Err(TargetError::Path(expected))
        );
    }

    #[test]
    fn malformed_link_paths_are_rejected_with_the_rule_they_break() {
        let cases: [(&str, LinkPathError); 5] = [
            ("", LinkPathError::Empty),
            (
                "/src/lib.rs",
                LinkPathError::Absolute {
                    path: "/src/lib.rs".to_owned(),
                },
            ),
            (
                "src//lib.rs",
                LinkPathError::EmptySegment {
                    path: "src//lib.rs".to_owned(),
                },
            ),
            (
                "src/",
                LinkPathError::EmptySegment {
                    path: "src/".to_owned(),
                },
            ),
            (
                "./src/lib.rs",
                LinkPathError::DotSegment {
                    path: "./src/lib.rs".to_owned(),
                },
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                raw.parse::<SourcePath>(),
                Err(expected.clone()),
                "source `{raw}`"
            );
            assert_eq!(
                raw.parse::<TargetPath>(),
                Err(expected),
                "target path `{raw}`"
            );
        }
    }

    #[test]
    fn source_from_relative_filesystem_path_uses_forward_slashes() {
        let built = SourcePath::from_relative(&Path::new("docs").join("guide").join("intro.md"))
            .expect("a plain relative path should convert");

        assert_eq!(built, source("docs/guide/intro.md"));
    }

    #[test]
    fn source_from_relative_rejects_rooted_and_parent_paths() {
        assert!(matches!(
            SourcePath::from_relative(Path::new("/repo/docs/a.md")),
            Err(LinkPathError::Absolute { .. }),
        ));
        assert!(matches!(
            SourcePath::from_relative(Path::new("../a.md")),
            Err(LinkPathError::ParentSegment { .. }),
        ));
    }

    #[test]
    fn namespaced_target_round_trips_as_written() {
        let parsed = target("auth:src/session.rs");

        assert_eq!(parsed.namespace().map(NamespaceName::as_str), Some("auth"));
        assert_eq!(parsed.path().as_str(), "src/session.rs");
        assert_eq!(parsed.to_string(), "auth:src/session.rs");
    }

    #[test]
    fn target_prefix_must_be_a_namespace_name() {
        assert!(matches!(
            "c:src/lib.rs".parse::<Target>(),
            Err(TargetError::Namespace(NamespaceNameError::TooShort { .. })),
        ));
    }

    #[test]
    fn plain_targets_sort_before_namespaced_targets() {
        let mut targets = [target("auth:a.rs"), target("src/lib.rs"), target("aa:z.rs")];
        targets.sort();

        assert_eq!(
            targets.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["src/lib.rs", "aa:z.rs", "auth:a.rs"],
        );
    }

    #[test]
    fn symbol_path_splits_on_dots_and_displays_joined() {
        let parsed = symbol("Config.Display.fmt");

        assert_eq!(parsed.segments(), ["Config", "Display", "fmt"]);
        assert_eq!(parsed.last(), "fmt");
        assert_eq!(parsed.to_string(), "Config.Display.fmt");
    }

    #[test]
    fn symbol_path_built_from_valid_segments_matches_parsing() {
        let segment = |name: &str| {
            SymbolSegment::new(name.to_owned()).expect("the fixture segment should be valid")
        };

        let built = SymbolPath::root(segment("Config")).child(segment("validate").numbered(1));

        assert_eq!(built, symbol("Config.validate-1"));
    }

    #[test]
    fn symbol_segment_rejects_empty_and_dotted_names() {
        for name in ["", "a.b"] {
            assert!(
                SymbolSegment::new(name.to_owned()).is_err(),
                "segment `{name}` should be rejected"
            );
        }
    }

    #[test]
    fn empty_symbol_segment_is_rejected() {
        for raw in ["Config..validate", ".validate", "Config."] {
            assert_eq!(
                raw.parse::<SymbolPath>().map_err(|error| error.to_string()),
                Err(format!("invalid symbol `{raw}`: a segment is empty")),
            );
        }
        assert_eq!(
            "".parse::<SymbolPath>().map_err(|error| error.reason),
            Err(SymbolPathError::EMPTY),
        );
    }

    #[test]
    fn symbol_path_from_segments_rejects_dots_inside_a_segment() {
        assert_eq!(
            SymbolPath::from_segments(vec!["v1.2".to_owned()]).map_err(|error| error.reason),
            Err(SymbolPathError::DOT_IN_SEGMENT),
        );
        assert_eq!(
            SymbolPath::from_segments(Vec::new()).map_err(|error| error.reason),
            Err(SymbolPathError::EMPTY),
        );
    }

    #[test]
    fn whole_file_keys_sort_before_symbol_keys() {
        let whole = LinkKey {
            source: source("docs/a.md"),
            target: target("src/lib.rs"),
            symbol: None,
        };
        let add = LinkKey {
            symbol: Some(symbol("add")),
            ..whole.clone()
        };

        assert!(
            whole < add,
            "a whole-file entry should sort before a symbol entry for the same target"
        );
    }

    #[test]
    fn link_key_displays_source_arrow_target_and_symbol() {
        let key = LinkKey {
            source: source("docs/guide.md"),
            target: target("src/lib.rs"),
            symbol: Some(symbol("Config.validate")),
        };

        assert_eq!(
            key.to_string(),
            "docs/guide.md → src/lib.rs#Config.validate"
        );
        assert_eq!(
            LinkKey {
                symbol: None,
                ..key
            }
            .to_string(),
            "docs/guide.md → src/lib.rs",
        );
    }

    #[test]
    fn link_ref_parses_each_documented_form() {
        let cases = [
            ("src/lib.rs", target("src/lib.rs"), None),
            ("src/lib.rs#add", target("src/lib.rs"), Some(symbol("add"))),
            (
                "auth:src/lib.rs#Config.port",
                target("auth:src/lib.rs"),
                Some(symbol("Config.port")),
            ),
        ];
        for (raw, target, symbol) in cases {
            let parsed: LinkRef = raw
                .parse()
                .unwrap_or_else(|error| panic!("`{raw}` should parse: {error}"));
            assert_eq!(parsed, LinkRef { target, symbol }, "`{raw}`");
            assert_eq!(parsed.to_string(), raw, "`{raw}` should display as written");
        }
    }

    #[test]
    fn link_ref_with_empty_symbol_is_rejected() {
        assert!(matches!(
            "src/lib.rs#".parse::<LinkRef>(),
            Err(LinkRefError::Symbol(_))
        ));
    }

    #[test]
    fn link_types_deserialize_through_their_checks() {
        #[derive(Debug, serde::Deserialize)]
        struct Entry {
            /// The source under test.
            source: SourcePath,
            /// The target under test.
            target: Target,
            /// The symbol under test.
            symbol: SymbolPath,
        }

        let entry: Entry = toml::from_str(
            "source = \"docs/guide.md\"\ntarget = \"auth:src/lib.rs\"\nsymbol = \"Config.port\"",
        )
        .expect("valid fields should deserialize");
        let bad_source = toml::from_str::<Entry>(
            "source = \"../guide.md\"\ntarget = \"src/lib.rs\"\nsymbol = \"add\"",
        );

        assert_eq!(entry.source, source("docs/guide.md"));
        assert_eq!(entry.target, target("auth:src/lib.rs"));
        assert_eq!(entry.symbol, symbol("Config.port"));
        assert!(
            bad_source.is_err(),
            "a source with `..` should fail to deserialize: {bad_source:?}"
        );
    }

    #[test]
    fn link_types_serialize_as_their_text() {
        let json =
            serde_json::to_string(&(source("docs/a.md"), target("auth:x.rs"), symbol("A.b")))
                .expect("link types should serialize");

        assert_eq!(json, r#"["docs/a.md","auth:x.rs","A.b"]"#);
    }
}
