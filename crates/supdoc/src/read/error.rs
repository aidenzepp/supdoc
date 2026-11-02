//! Errors from reading one file ([`ReadError`]) and from resolving a symbol in it
//! ([`ResolveError`]) (03 R13, Errors).
//!
//! Each error has a stable `code()` and a `help()` that says what to do next. Both are about one
//! file, so callers turn them into findings rather than stopping the run.

use std::fmt::{self, Display, Formatter};

use crate::link::SymbolPath;
use crate::lock::error::shell_quote;
use crate::path::AbsolutePath;
use crate::read::query::QueryFileError;

/// Why a file couldn't be read and parsed.
#[derive(Debug)]
pub enum ReadError {
    /// The file doesn't exist.
    Missing {
        /// The file that was looked for.
        path: AbsolutePath,
    },

    /// The filesystem refused the read.
    Io {
        /// The file being read.
        path: AbsolutePath,
        /// What the operating system reported.
        error: std::io::Error,
    },

    /// The file isn't UTF-8 (03 R13).
    NotUtf8 {
        /// The file being read.
        path: AbsolutePath,
        /// Byte offset of the first byte that isn't part of valid UTF-8.
        offset: usize,
    },

    /// The file is over the size limit (03 R13).
    TooLarge {
        /// The file being read.
        path: AbsolutePath,
        /// Its size in bytes.
        size: u64,
        /// The largest size supdoc reads.
        limit: u64,
    },

    /// No cartridge reads files with this extension.
    UnsupportedLanguage {
        /// The file being read.
        path: AbsolutePath,
        /// Its extension, without the dot; empty when the file has none.
        extension: String,
        /// Every extension supdoc can read, sorted, for the help text.
        supported: Vec<&'static str>,
    },

    /// A condition supdoc guarantees cannot happen did (tree-sitter producing no tree, a symbol
    /// whose range lies outside its file), so this is a bug.
    Internal {
        /// The file being read.
        path: AbsolutePath,
        /// What went wrong, as a short phrase that reads before "for <file>".
        reason: &'static str,
    },
}

/// A built-in cartridge that cannot be loaded: a supdoc bug that fails the run (exit 70), not a
/// problem with any one file.
#[derive(Debug, thiserror::Error)]
pub enum CartridgeLoadError {
    /// A built-in query file doesn't compile against its grammar.
    #[error("{0}")]
    Query(&'static QueryFileError),

    /// The Markdown inline grammar can't be loaded into a parser.
    #[error("the built-in Markdown inline grammar does not load (supdoc bug): {0:?}")]
    InlineGrammar(&'static tree_sitter::LanguageError),
}

impl CartridgeLoadError {
    /// The stable code: `read-cartridge-load`.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        "read-cartridge-load"
    }

    /// What to do next.
    #[must_use]
    pub fn help(&self) -> String {
        concat!(
            "Reinstall supdoc with `cargo install --locked supdoc`. If it still fails, report a \
             bug at ",
            env!("CARGO_PKG_REPOSITORY"),
            "/issues, including the output of `supdoc --version`"
        )
        .to_owned()
    }
}

/// What a file was being read as, which decides what advice fits its [`ReadError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadRole {
    /// A walked source file being scanned for links; `exclude` removes it from the scan.
    ScannedSource,
    /// A walked source file being renamed past, which must never be offered `exclude`.
    RenamedSource,
    /// A file a link points at; `exclude` does not matter to a link.
    Target,
    /// The `FILE` argument of `supdoc resolve`, read whatever `exclude` says.
    ResolveArgument,
}

impl ReadError {
    /// The stable code for this error: `read-missing`, `read-not-utf8`, …
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing { .. } => "read-missing",
            Self::Io { .. } => "read-io",
            Self::NotUtf8 { .. } => "read-not-utf8",
            Self::TooLarge { .. } => "read-too-large",
            Self::UnsupportedLanguage { .. } => "read-unsupported-language",
            Self::Internal { .. } => "read-internal",
        }
    }

    /// The file that couldn't be read.
    #[must_use]
    pub fn path(&self) -> &AbsolutePath {
        match self {
            Self::Missing { path }
            | Self::Io { path, .. }
            | Self::NotUtf8 { path, .. }
            | Self::TooLarge { path, .. }
            | Self::UnsupportedLanguage { path, .. }
            | Self::Internal { path, .. } => path,
        }
    }

    /// The message, naming the file as `shown` instead of by its absolute path.
    #[must_use]
    pub fn message_naming(&self, shown: &dyn Display) -> String {
        match self {
            Self::Missing { .. } => format!("{shown} does not exist"),
            Self::Io { error, .. } => format!("cannot read {shown}: {error}"),
            Self::NotUtf8 { offset, .. } => {
                format!("{shown} is not UTF-8 (first invalid byte at offset {offset})")
            }
            Self::TooLarge { size, limit, .. } => {
                format!("{shown} is {size} bytes; the limit is {limit}")
            }
            Self::UnsupportedLanguage { extension, .. } => {
                format!("no language for `.{extension}` files ({shown})")
            }
            Self::Internal { reason, .. } => {
                format!("{reason} for {shown} (supdoc bug)")
            }
        }
    }

    /// What to do next, for a file read in `role`, with the file named as `shown`.
    ///
    /// Only a file that is missing, not UTF-8 or too large reads differently by role; the
    /// other errors give the same advice whoever asked.
    #[must_use]
    pub fn help(&self, role: ReadRole, shown: &str) -> String {
        match (self, role) {
            (Self::Missing { .. }, ReadRole::Target) => {
                format!("If `{shown}` moved, update the link to its new path")
            }
            (Self::Missing { .. }, _) => {
                "If the file moved, update the link to its new path".to_owned()
            }
            (Self::NotUtf8 { .. }, ReadRole::ScannedSource) => {
                "Save the file as UTF-8, or add it to `exclude` in `supdoc.config.toml`".to_owned()
            }
            (Self::NotUtf8 { .. }, ReadRole::RenamedSource) => "Save the file as UTF-8".to_owned(),
            (Self::NotUtf8 { .. }, ReadRole::Target) => {
                format!("Save `{shown}` as UTF-8, or point the link at a file that is")
            }
            (Self::NotUtf8 { .. }, ReadRole::ResolveArgument) => {
                format!("Save `{shown}` as UTF-8, or resolve a file that is")
            }
            (Self::TooLarge { limit, .. }, ReadRole::ScannedSource) => format!(
                "Shrink the file under `{limit}` bytes, or add it to `exclude` in \
                 `supdoc.config.toml`"
            ),
            (Self::TooLarge { limit, .. }, ReadRole::RenamedSource) => {
                format!("Shrink the file under `{limit}` bytes")
            }
            (Self::TooLarge { limit, .. }, ReadRole::ResolveArgument) => {
                format!("Resolve a file under `{limit}` bytes")
            }
            (Self::TooLarge { limit, .. }, ReadRole::Target) => format!(
                "Link to a symbol in a file under `{limit}` bytes. Generated files usually \
                 shouldn't be linked"
            ),
            (Self::Io { .. }, _) => {
                "Check that the current user can read the file, for example with `chmod u+r` on it"
                    .to_owned()
            }
            (Self::UnsupportedLanguage { supported, .. }, _) => {
                let listed = supported
                    .iter()
                    .map(|extension| format!("`.{extension}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "supdoc reads {listed}. A whole-file link to this type can't be tracked yet"
                )
            }
            (Self::Internal { .. }, _) => concat!(
                "Report a bug at ",
                env!("CARGO_PKG_REPOSITORY"),
                "/issues, including the output of `supdoc --version` and the file's \
                     language and size"
            )
            .to_owned(),
        }
    }
}

/// Whether `query` is a GitHub line anchor: `L<n>` or `L<n>-L<m>`.
fn is_line_anchor(query: &str) -> bool {
    let digits = |part: &str| {
        part.strip_prefix('L')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|byte| byte.is_ascii_digit()))
    };
    query.split_once('-').map_or_else(
        || digits(query),
        |(start, end)| digits(start) && digits(end),
    )
}

impl Display for ReadError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message_naming(&self.path().display()))
    }
}

impl std::error::Error for ReadError {}

/// Why a symbol path names no single symbol in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Nothing in the file has that path.
    NotFound {
        /// The file searched.
        file: AbsolutePath,
        /// The path asked for.
        query: SymbolPath,
        /// Up to three paths closest to `query` by edit distance, closest first.
        nearest: Vec<SymbolPath>,
        /// Whether the file has any symbols at all.
        file_has_symbols: bool,
    },

    /// Several symbols answer to that path; supdoc never picks one (03 R5a).
    Ambiguous {
        /// The file searched.
        file: AbsolutePath,
        /// The path asked for.
        query: SymbolPath,
        /// Every match in file order: its full path, or its first alias when it has one.
        candidates: Vec<SymbolPath>,
    },
}

impl ResolveError {
    /// The stable code for this error: `resolve-not-found` or `resolve-ambiguous`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "resolve-not-found",
            Self::Ambiguous { .. } => "resolve-ambiguous",
        }
    }

    /// What to do next, naming the file by its absolute path.
    #[must_use]
    pub fn help(&self) -> String {
        self.help_running(&format!(
            "supdoc resolve {}",
            shell_quote(&self.file().display().to_string())
        ))
    }

    /// The message, naming the file as `shown` instead of by its absolute path.
    #[must_use]
    pub fn message_naming(&self, shown: &dyn Display) -> String {
        match self {
            Self::NotFound { query, .. } => format!("no symbol `{query}` in {shown}"),
            Self::Ambiguous {
                query, candidates, ..
            } => format!("`{query}` matches {} symbols in {shown}", candidates.len()),
        }
    }

    /// What to do next, with `command` as the whole shell command that lists the file's symbols,
    /// so a run that was given `-c` or a relative path repeats both.
    #[must_use]
    pub fn help_running(&self, command: &str) -> String {
        self.help_listing(&format!("`{command}` lists every symbol"))
    }

    /// What to do next, for a `shown` path relative to the project root. A scan cannot know the
    /// caller's directory, so the hint says where to run the command.
    #[must_use]
    pub fn help_from_root(&self, shown: &dyn Display) -> String {
        self.help_listing(&format!(
            "run `supdoc resolve {}` from the project root to list its symbols",
            shell_quote(&shown.to_string())
        ))
    }

    /// The help text, with `list` as the sentence that says how to list the file's symbols.
    fn help_listing(&self, list: &str) -> String {
        match self {
            Self::NotFound {
                query,
                nearest,
                file_has_symbols,
                ..
            } => {
                if is_line_anchor(&query.to_string()) {
                    format!(
                        "line anchors like `#{query}` aren't supported. link a symbol name, or \
                         the whole file without a `#`. {list}"
                    )
                } else if !file_has_symbols {
                    "the file has no symbols. link to the whole file, without a `#`".to_owned()
                } else if nearest.is_empty() {
                    format!("no symbol has a similar name. {list}")
                } else {
                    format!("did you mean {}? {list}", quoted(nearest))
                }
            }
            Self::Ambiguous { candidates, .. } => {
                let first: Vec<SymbolPath> =
                    candidates.iter().take(AMBIGUOUS_LISTED).cloned().collect();
                let more = candidates.len().saturating_sub(AMBIGUOUS_LISTED);
                let listed = quoted(&first);
                if more == 0 {
                    format!("write one of the full paths: {listed}")
                } else {
                    format!("write one of the full paths: {listed}, and {more} more; {list}")
                }
            }
        }
    }

    /// The file searched.
    #[must_use]
    pub fn file(&self) -> &AbsolutePath {
        match self {
            Self::NotFound { file, .. } | Self::Ambiguous { file, .. } => file,
        }
    }
}

impl Display for ResolveError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message_naming(&self.file().display()))
    }
}

impl std::error::Error for ResolveError {}

/// How many ambiguous candidates the help names before summarizing the rest.
const AMBIGUOUS_LISTED: usize = 5;

/// `` `a`, `b`, `c` ``
fn quoted(paths: &[SymbolPath]) -> String {
    paths
        .iter()
        .map(|path| format!("`{path}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn absolute(raw: &str) -> AbsolutePath {
        AbsolutePath::new(Path::new(raw)).expect("the fixture path should be absolute")
    }

    fn symbol(raw: &str) -> SymbolPath {
        raw.parse().expect("the fixture symbol should be valid")
    }

    #[test]
    fn not_found_names_the_query_and_suggests_nearest_paths() {
        let error = ResolveError::NotFound {
            file: absolute("/repo/src/lib.rs"),
            query: symbol("Config.fmt"),
            nearest: vec![symbol("Config.Display.fmt"), symbol("Config.port")],
            file_has_symbols: true,
        };

        assert_eq!(error.code(), "resolve-not-found");
        assert_eq!(
            error.to_string(),
            "no symbol `Config.fmt` in /repo/src/lib.rs"
        );
        assert_eq!(
            error.help(),
            "did you mean `Config.Display.fmt`, `Config.port`? \
             `supdoc resolve /repo/src/lib.rs` lists every symbol",
        );
    }

    #[test]
    fn not_found_explains_line_anchors_and_symbol_free_files() {
        let not_found = |query: &str, file_has_symbols| ResolveError::NotFound {
            file: absolute("/repo/src/lib.rs"),
            query: symbol(query),
            nearest: Vec::new(),
            file_has_symbols,
        };

        assert!(
            not_found("L10", true)
                .help()
                .starts_with("line anchors like `#L10` aren't supported")
        );
        assert!(
            not_found("L10-L12", true)
                .help()
                .contains("aren't supported")
        );
        assert_eq!(
            not_found("foo", false).help(),
            "the file has no symbols. link to the whole file, without a `#`"
        );
        assert!(
            not_found("foo", true)
                .help()
                .starts_with("no symbol has a similar name.")
        );
    }

    #[test]
    fn ambiguous_lists_every_full_path() {
        let error = ResolveError::Ambiguous {
            file: absolute("/repo/src/lib.rs"),
            query: symbol("new"),
            candidates: vec![symbol("Foo.new"), symbol("Bar.new")],
        };

        assert_eq!(error.code(), "resolve-ambiguous");
        assert_eq!(
            error.to_string(),
            "`new` matches 2 symbols in /repo/src/lib.rs"
        );
        assert_eq!(
            error.help(),
            "write one of the full paths: `Foo.new`, `Bar.new`"
        );
    }

    #[test]
    fn unsupported_language_help_lists_supported_extensions() {
        let error = ReadError::UnsupportedLanguage {
            path: absolute("/repo/logo.png"),
            extension: "png".to_owned(),
            supported: vec!["md", "rs"],
        };

        assert_eq!(error.code(), "read-unsupported-language");
        assert_eq!(
            error.to_string(),
            "no language for `.png` files (/repo/logo.png)"
        );
        assert_eq!(
            error.help(ReadRole::Target, "logo.png"),
            "supdoc reads `.md`, `.rs`. A whole-file link to this type can't be tracked yet",
        );
    }

    #[test]
    fn too_large_source_help_gives_source_advice_not_link_advice() {
        let error = ReadError::TooLarge {
            path: absolute("/repo/big.md"),
            size: 18_000_000,
            limit: 16_777_216,
        };

        let scanned = error.help(ReadRole::ScannedSource, "big.md");
        assert!(
            scanned.contains("exclude") && !scanned.contains("Link to a symbol"),
            "a too-large source should be told to shrink or exclude it, got: {scanned}"
        );
        let renamed = error.help(ReadRole::RenamedSource, "big.md");
        assert!(
            !renamed.contains("exclude") && !renamed.contains("Link to a symbol"),
            "a too-large renamed source must not be offered exclude, got: {renamed}"
        );
        assert!(
            error
                .help(ReadRole::Target, "big.md")
                .contains("Link to a symbol"),
            "a too-large target keeps the link advice"
        );
    }
}
