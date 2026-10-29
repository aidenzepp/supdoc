//! Reading a Markdown link destination into a link target and an optional symbol (03 R10, R11).
//!
//! The rules run in a fixed order, and each one either settles the destination or hands it on:
//!
//! 1. `scheme://...`, `mailto:`, `tel:`, `data:` and `name::path` are not supdoc links.
//! 2. Any other `prefix:` is a configured namespace or an [`LinkError::UnknownNamespace`].
//! 3. A leading `/` is the project root; anything else is relative to the source file's directory.
//! 4. Without a `#fragment`, only a file some cartridge reads is a (whole-file) link.
//! 5. With a fragment, `#heading` alone (the same file) is not tracked; otherwise it is a symbol.
//!
//! Paths and fragments are percent-decoded; the prefix and the `scheme://` test are not.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

use crate::link::{
    LinkPathError, RawLink, SourcePath, SymbolPath, SymbolPathError, Target, TargetPath,
};
use crate::namespace::NamespaceName;
use crate::path::AbsolutePath;
use crate::read::cartridge::Languages;

/// Schemes that name no path and never take `//`.
const IGNORED_SCHEMES: [&str; 3] = ["mailto", "tel", "data"];

/// How a text fragment (`#add:~:text=...`) is marked inside a fragment.
const TEXT_FRAGMENT_MARKER: &str = ":~:";

/// The reason given when a fragment is not UTF-8 once percent-decoded.
const FRAGMENT_NOT_UTF8: &str = "it is not valid UTF-8 after percent-decoding";

/// A destination understood as a supdoc link: the file it points at and, optionally, a symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLink {
    /// The file the link points at, as it will be written in the lockfile.
    pub target: Target,
    /// The symbol inside that file, or `None` for a whole-file link.
    pub symbol: Option<SymbolPath>,
}

/// Why a destination that looks like a supdoc link cannot be read.
///
/// One link's problem, not the file's: callers report it as a finding and keep going.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    /// The prefix before `:` is not a configured namespace (and is not a URL or `name::path`).
    #[error("unknown namespace `{prefix}` in `{destination}`")]
    UnknownNamespace {
        /// The text before the first `:`.
        prefix: String,
        /// The destination as written.
        destination: String,
        /// Every configured namespace, nearest spelling to `prefix` first.
        known: Vec<NamespaceName>,
    },
    /// The path part is not a valid link path (empty, `a//b`, `\`, not UTF-8, ...).
    #[error("invalid path in `{destination}`: {error}")]
    InvalidPath {
        /// The destination as written.
        destination: String,
        /// The rule the path breaks.
        error: LinkPathError,
    },
    /// The part after `#` is not a valid symbol path.
    #[error("invalid symbol `{fragment}`: {reason}")]
    InvalidSymbol {
        /// The fragment as written, without the `#`.
        fragment: String,
        /// What is wrong with it, as a phrase (`a segment is empty`).
        reason: &'static str,
    },
    /// The fragment carries a text fragment (`:~:`), which is not built yet.
    #[error("text fragments (`:~:`) aren't supported yet")]
    TextFragmentUnsupported {
        /// The destination as written.
        destination: String,
    },
}

/// Reads `raw`'s destination as written in `source`.
///
/// Returns `None` when the destination is not a supdoc link: a URL, `mailto:`-style scheme,
/// `name::path` language path, same-file `#anchor`, or a whole-file link to a file no cartridge in
/// `languages` reads (`logo.png`). `Some(Err(_))` is a link that is meant for supdoc but is wrong.
///
/// Pure: it never touches the filesystem, so a link to a missing file is still `Some(Ok(_))`.
#[must_use]
pub fn parse_destination(
    raw: &RawLink,
    source: &SourcePath,
    namespaces: &BTreeMap<NamespaceName, AbsolutePath>,
    languages: &Languages,
) -> Option<Result<ParsedLink, LinkError>> {
    let destination = raw.destination.trim();
    let (before_fragment, fragment) = match destination.split_once('#') {
        Some((before, fragment)) => (before, Some(fragment)),
        None => (destination, None),
    };

    if is_not_a_path(before_fragment) {
        return None;
    }
    let (namespace, path) = match split_prefix(before_fragment) {
        Some((prefix, path)) => match namespaces_entry(namespaces, prefix) {
            Some(namespace) => (Some(namespace), path),
            None => return Some(Err(unknown_namespace(prefix, destination, namespaces))),
        },
        None => (None, before_fragment),
    };

    let fragment = fragment.filter(|fragment| !fragment.is_empty());
    if path.is_empty() && namespace.is_none() {
        // `#setup` (or an empty destination): a heading in this very file, never tracked (R11).
        return None;
    }

    let Some(decoded_path) = percent_decode(path) else {
        return Some(Err(LinkError::InvalidPath {
            destination: destination.to_owned(),
            error: LinkPathError::NotUtf8 {
                path: path.to_owned(),
            },
        }));
    };
    let relative = match (&namespace, decoded_path.strip_prefix('/')) {
        (Some(_), _) => decoded_path,
        (None, Some(from_root)) => from_root.to_owned(),
        (None, None) => join_to_source_directory(source, &decoded_path),
    };

    if fragment.is_none()
        && !relative.is_empty()
        && languages.for_path(Path::new(&relative)).is_none()
    {
        return None;
    }

    Some(build(destination, namespace, &relative, fragment))
}

/// Builds the link from its already-separated, already-routed parts.
///
/// # Errors
///
/// Fails if the symbol or the path is invalid; the symbol is checked first.
fn build(
    destination: &str,
    namespace: Option<NamespaceName>,
    relative: &str,
    fragment: Option<&str>,
) -> Result<ParsedLink, LinkError> {
    let symbol = fragment
        .map(|fragment| parse_symbol(destination, fragment))
        .transpose()?;
    let path =
        TargetPath::try_from(relative.to_owned()).map_err(|error| LinkError::InvalidPath {
            destination: destination.to_owned(),
            error,
        })?;
    let target = match namespace {
        Some(namespace) => Target::Namespaced { namespace, path },
        None => Target::Path(path),
    };
    Ok(ParsedLink { target, symbol })
}

/// Percent-decodes `fragment` and reads it as a dotted symbol path.
///
/// # Errors
///
/// Fails if the fragment is not UTF-8, carries a text fragment, or is not a valid symbol path.
fn parse_symbol(destination: &str, fragment: &str) -> Result<SymbolPath, LinkError> {
    let invalid = |reason| LinkError::InvalidSymbol {
        fragment: fragment.to_owned(),
        reason,
    };
    let decoded = percent_decode(fragment).ok_or_else(|| invalid(FRAGMENT_NOT_UTF8))?;
    if decoded.contains(TEXT_FRAGMENT_MARKER) {
        return Err(LinkError::TextFragmentUnsupported {
            destination: destination.to_owned(),
        });
    }
    SymbolPath::from_str(&decoded).map_err(|SymbolPathError { reason, .. }| invalid(reason))
}

/// Whether `text` (a destination without its fragment) names no path at all: a URL, an ignored
/// scheme, or a language path such as `crate::Foo` or `Self::iter` (R10).
fn is_not_a_path(text: &str) -> bool {
    if text.starts_with("::") {
        return true;
    }
    let Some((prefix, rest)) = text.split_once(':') else {
        return false;
    };
    has_scheme_shape(prefix) && rest.starts_with("//")
        || IGNORED_SCHEMES
            .iter()
            .any(|scheme| prefix.eq_ignore_ascii_case(scheme))
        || !prefix.contains('/') && rest.starts_with(':')
}

/// Whether `prefix` could be a URL scheme: a letter, then letters, digits, `+`, `-` or `.`.
fn has_scheme_shape(prefix: &str) -> bool {
    let mut chars = prefix.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Splits `prefix:path` when `prefix` is written before any `/`; `docs/a:b.md` has no prefix.
fn split_prefix(text: &str) -> Option<(&str, &str)> {
    let (prefix, path) = text.split_once(':')?;
    (!prefix.contains('/')).then_some((prefix, path))
}

/// The configured namespace spelled `prefix`, if any.
fn namespaces_entry(
    namespaces: &BTreeMap<NamespaceName, AbsolutePath>,
    prefix: &str,
) -> Option<NamespaceName> {
    let name = NamespaceName::from_str(prefix).ok()?;
    namespaces.contains_key(&name).then_some(name)
}

/// The error for `prefix`, listing the configured namespaces nearest first.
fn unknown_namespace(
    prefix: &str,
    destination: &str,
    namespaces: &BTreeMap<NamespaceName, AbsolutePath>,
) -> LinkError {
    let mut known: Vec<NamespaceName> = namespaces.keys().cloned().collect();
    // The sort is stable, so equally near names stay in alphabetical order.
    known.sort_by_key(|name| strsim::levenshtein(prefix, name.as_str()));
    LinkError::UnknownNamespace {
        prefix: prefix.to_owned(),
        destination: destination.to_owned(),
        known,
    }
}

/// Resolves `relative` against the directory holding `source`, lexically, keeping a leading `..`
/// when the path climbs out of the project root (a target may, 02 L5).
///
/// Empty segments are kept so [`TargetPath`] rejects them with its own message.
fn join_to_source_directory(source: &SourcePath, relative: &str) -> String {
    let mut segments: Vec<&str> = source.as_str().split('/').collect();
    let _file_name = segments.pop();
    for segment in relative.split('/') {
        match segment {
            "." => {}
            ".." if segments.last().is_some_and(|last| *last != "..") => {
                let _parent = segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// Decodes `%XX` escapes. A `%` not followed by two hex digits stays as written, as in `CommonMark`.
/// `None` when the decoded bytes are not UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut remaining = text.bytes();
    while let Some(byte) = remaining.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let mut lookahead = remaining.clone();
        let decoded = lookahead
            .next()
            .and_then(hex_value)
            .zip(lookahead.next().and_then(hex_value))
            .map(|(high, low)| high * 16 + low);
        match decoded {
            Some(value) => {
                bytes.push(value);
                remaining = lookahead;
            }
            None => bytes.push(byte),
        }
    }
    String::from_utf8(bytes).ok()
}

/// The value of one ASCII hex digit.
fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;
    use crate::read::cartridge::{Cartridge, CompiledQueries, LanguageId, SourceFile, SymbolTable};

    /// A data-only cartridge that claims `extensions`; only its extensions matter here.
    #[derive(Debug)]
    struct ExtensionOnly {
        /// The extensions it claims.
        extensions: &'static [&'static str],
        /// The grammar, created on first use.
        grammar: OnceLock<tree_sitter::Language>,
        /// Empty queries, compiled on first use.
        queries: OnceLock<CompiledQueries>,
    }

    impl Cartridge for ExtensionOnly {
        fn id(&self) -> LanguageId {
            LanguageId::new("test")
        }

        fn extensions(&self) -> &'static [&'static str] {
            self.extensions
        }

        fn grammar(&self) -> &tree_sitter::Language {
            self.grammar
                .get_or_init(|| tree_sitter_rust::LANGUAGE.into())
        }

        fn queries(&self) -> &CompiledQueries {
            self.queries.get_or_init(|| {
                let empty = || {
                    tree_sitter::Query::new(self.grammar(), "")
                        .expect("an empty query should compile against any grammar")
                };
                CompiledQueries::new(empty(), empty())
            })
        }

        fn symbols(&self, _file: &SourceFile) -> SymbolTable {
            SymbolTable::default()
        }

        fn markdown_regions(&self, _file: &SourceFile) -> Vec<tree_sitter::Range> {
            Vec::new()
        }
    }

    static READS_RS_AND_MD: ExtensionOnly = ExtensionOnly {
        extensions: &["rs", "md"],
        grammar: OnceLock::new(),
        queries: OnceLock::new(),
    };

    /// Parses `destination` written in `docs/guide.md` with namespaces `auth` and `billing`.
    fn parse(destination: &str) -> Option<Result<ParsedLink, LinkError>> {
        parse_in("docs/guide.md", destination)
    }

    /// Parses `destination` written in `source`, with namespaces `auth` and `billing`.
    fn parse_in(source: &str, destination: &str) -> Option<Result<ParsedLink, LinkError>> {
        let namespaces = ["auth", "billing"]
            .into_iter()
            .map(|name| {
                let dir = AbsolutePath::new(Path::new("/elsewhere"))
                    .expect("the fixture directory is absolute");
                (
                    name.parse::<NamespaceName>()
                        .expect("the fixture namespace name should be valid"),
                    dir,
                )
            })
            .collect();
        let raw = RawLink {
            text: "text".to_owned(),
            destination: destination.to_owned(),
            range: 0..destination.len(),
        };
        let source: SourcePath = source
            .parse()
            .expect("the fixture source path should be valid");
        parse_destination(
            &raw,
            &source,
            &namespaces,
            &Languages::new(vec![&READS_RS_AND_MD]),
        )
    }

    /// The link a destination should parse to, from its target text and symbol text.
    fn link(target: &str, symbol: Option<&str>) -> ParsedLink {
        ParsedLink {
            target: target.parse().expect("the expected target should be valid"),
            symbol: symbol
                .map(|symbol| symbol.parse().expect("the expected symbol should be valid")),
        }
    }

    #[test]
    fn url_and_mailto_destinations_are_ignored() {
        for destination in [
            "https://example.com/a.rs#x",
            "http://example.com",
            "ftp://host/a.rs",
            "mailto:a@b.co",
            "MAILTO:a@b.co",
            "tel:+15550100",
            "data:text/plain;base64,AAAA",
        ] {
            assert_eq!(parse(destination), None, "{destination}");
        }
    }

    #[test]
    fn rustdoc_intra_doc_links_are_not_supdoc_links() {
        for destination in [
            "crate::Foo",
            "Self::iter",
            "std::collections::HashMap",
            "::foo",
        ] {
            assert_eq!(parse(destination), None, "{destination}");
        }
    }

    #[test]
    fn configured_prefix_is_a_namespaced_target() {
        assert_eq!(
            parse("auth:src/lib.rs#login"),
            Some(Ok(link("auth:src/lib.rs", Some("login")))),
        );
    }

    #[test]
    fn namespaced_path_is_not_joined_to_the_source_directory() {
        assert_eq!(
            parse_in("a/b/c.md", "auth:../x.rs"),
            Some(Ok(link("auth:../x.rs", None))),
        );
    }

    #[test]
    fn unconfigured_prefix_is_unknown_namespace_with_nearest() {
        let names = |list: &[&str]| -> Vec<NamespaceName> {
            list.iter()
                .map(|name| name.parse().expect("the fixture name should be valid"))
                .collect()
        };
        assert_eq!(
            parse("bilng:src/x.rs#f"),
            Some(Err(LinkError::UnknownNamespace {
                prefix: "bilng".to_owned(),
                destination: "bilng:src/x.rs#f".to_owned(),
                known: names(&["billing", "auth"]),
            })),
        );
    }

    #[test]
    fn colon_in_the_path_part_is_an_invalid_path() {
        assert_eq!(
            parse("src/a:b.rs"),
            Some(Err(LinkError::InvalidPath {
                destination: "src/a:b.rs".to_owned(),
                error: LinkPathError::Colon {
                    path: "docs/src/a:b.rs".to_owned(),
                },
            })),
        );
    }

    #[test]
    fn leading_slash_is_project_root() {
        assert_eq!(
            parse_in("docs/deep/guide.md", "/src/lib.rs#add"),
            Some(Ok(link("src/lib.rs", Some("add")))),
        );
    }

    #[test]
    fn relative_path_is_joined_to_the_source_directory() {
        assert_eq!(
            parse_in("docs/guide.md", "../src/./lib.rs#add"),
            Some(Ok(link("src/lib.rs", Some("add")))),
        );
        assert_eq!(
            parse_in("guide.md", "src/lib.rs"),
            Some(Ok(link("src/lib.rs", None)))
        );
    }

    #[test]
    fn relative_path_may_climb_out_of_the_project_root() {
        assert_eq!(
            parse_in("docs/guide.md", "../../shared/x.rs"),
            Some(Ok(link("../shared/x.rs", None))),
        );
        assert_eq!(
            parse_in("guide.md", "../../x.rs"),
            Some(Ok(link("../../x.rs", None))),
        );
    }

    #[test]
    fn percent_encoding_is_decoded() {
        assert_eq!(
            parse("../my%20src/lib.rs#a%2Eb"),
            Some(Ok(link("my src/lib.rs", Some("a.b")))),
        );
    }

    #[test]
    fn malformed_percent_escape_is_kept_as_written() {
        assert_eq!(
            parse("../100%/x.rs#a%zz"),
            Some(Ok(link("100%/x.rs", Some("a%zz"))))
        );
    }

    #[test]
    fn percent_escapes_that_are_not_utf8_are_an_error() {
        assert!(matches!(
            parse("../%FF.rs"),
            Some(Err(LinkError::InvalidPath {
                error: LinkPathError::NotUtf8 { .. },
                ..
            })),
        ));
        assert_eq!(
            parse("../x.rs#%FF"),
            Some(Err(LinkError::InvalidSymbol {
                fragment: "%FF".to_owned(),
                reason: FRAGMENT_NOT_UTF8,
            })),
        );
    }

    #[test]
    fn plain_link_to_doc_is_a_whole_file_link() {
        assert_eq!(parse("setup.md"), Some(Ok(link("docs/setup.md", None))));
    }

    #[test]
    fn same_file_anchor_is_not_tracked() {
        assert_eq!(parse("#setup"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn whole_file_link_to_unsupported_type_is_not_tracked() {
        assert_eq!(parse("logo.png"), None);
        assert_eq!(parse("../src/"), None);
    }

    #[test]
    fn symbol_link_is_kept_even_for_a_file_no_cartridge_reads() {
        assert_eq!(
            parse("logo.png#x"),
            Some(Ok(link("docs/logo.png", Some("x"))))
        );
    }

    #[test]
    fn empty_fragment_is_a_whole_file_link() {
        assert_eq!(parse("../src/lib.rs#"), Some(Ok(link("src/lib.rs", None))));
    }

    #[test]
    fn symbol_with_empty_segment_is_invalid() {
        assert_eq!(
            parse("../src/lib.rs#a..b"),
            Some(Err(LinkError::InvalidSymbol {
                fragment: "a..b".to_owned(),
                reason: SymbolPathError::EMPTY_SEGMENT,
            })),
        );
    }

    #[test]
    fn text_fragment_is_reported_not_parsed_as_a_symbol() {
        assert_eq!(
            parse("../src/lib.rs#add:~:text=hello"),
            Some(Err(LinkError::TextFragmentUnsupported {
                destination: "../src/lib.rs#add:~:text=hello".to_owned(),
            })),
        );
    }

    #[test]
    fn invalid_paths_are_reported_with_the_destination() {
        for (destination, error) in [
            ("/", LinkPathError::Empty),
            (
                "../a//b.rs",
                LinkPathError::EmptySegment {
                    path: "a//b.rs".to_owned(),
                },
            ),
            (
                "auth:a\\b.rs",
                LinkPathError::Backslash {
                    path: "a\\b.rs".to_owned(),
                },
            ),
        ] {
            assert_eq!(
                parse(destination),
                Some(Err(LinkError::InvalidPath {
                    destination: destination.to_owned(),
                    error,
                })),
                "{destination}",
            );
        }
    }
}
