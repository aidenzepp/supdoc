//! Errors from reading one file ([`ReadError`]) and from resolving a symbol in it
//! ([`ResolveError`]) (03 R13, Errors).
//!
//! Each error has a stable `code()` and a `help()` that says what to do next. Both are about one
//! file, so callers turn them into findings rather than stopping the run.

use std::fmt::{self, Display, Formatter};

use crate::link::SymbolPath;
use crate::path::AbsolutePath;

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

    /// tree-sitter produced no tree. supdoc sets no timeout or cancellation, so this is a bug.
    Internal {
        /// The file being parsed.
        path: AbsolutePath,
    },
}

impl ReadError {
    /// The stable code for this error: `read/missing`, `read/not-utf8`, …
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing { .. } => "read/missing",
            Self::Io { .. } => "read/io",
            Self::NotUtf8 { .. } => "read/not-utf8",
            Self::TooLarge { .. } => "read/too-large",
            Self::UnsupportedLanguage { .. } => "read/unsupported-language",
            Self::Internal { .. } => "read/internal",
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
            | Self::Internal { path } => path,
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
            Self::Internal { .. } => {
                format!("tree-sitter returned no tree for {shown} (supdoc bug)")
            }
        }
    }

    /// What to do next.
    #[must_use]
    pub fn help(&self) -> String {
        match self {
            Self::Missing { .. } => {
                "If the file moved, update the link. `supdoc fix` can do it when \
                                     the symbol still exists elsewhere"
                    .to_owned()
            }
            Self::Io { .. } => "Check that the current user can read the file".to_owned(),
            Self::NotUtf8 { .. } => "Save the file as UTF-8, or exclude it".to_owned(),
            Self::TooLarge { .. } => "Link to a symbol in a smaller file. Generated files usually \
                                      shouldn't be linked"
                .to_owned(),
            Self::UnsupportedLanguage { supported, .. } => {
                let listed = supported
                    .iter()
                    .map(|extension| format!(".{extension}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "supdoc reads {listed}. A whole-file link to this type can't be tracked yet"
                )
            }
            Self::Internal { .. } => {
                "Report a bug, including the file's language and size".to_owned()
            }
        }
    }
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
    },

    /// Several symbols answer to that path; supdoc never picks one (03 R5a).
    Ambiguous {
        /// The file searched.
        file: AbsolutePath,
        /// The path asked for.
        query: SymbolPath,
        /// The full path of every match, in file order.
        candidates: Vec<SymbolPath>,
    },
}

impl ResolveError {
    /// The stable code for this error: `resolve/not-found` or `resolve/ambiguous`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "resolve/not-found",
            Self::Ambiguous { .. } => "resolve/ambiguous",
        }
    }

    /// What to do next, naming the file by its absolute path.
    #[must_use]
    pub fn help(&self) -> String {
        self.help_naming(&self.file().display())
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

    /// What to do next, naming the file as `shown` instead of by its absolute path.
    #[must_use]
    pub fn help_naming(&self, shown: &dyn Display) -> String {
        match self {
            Self::NotFound { nearest, .. } => {
                let list = format!("`supdoc resolve {shown}` lists every symbol");
                if nearest.is_empty() {
                    format!("the file has no symbols with a similar name. {list}")
                } else {
                    format!("did you mean {}? {list}", quoted(nearest))
                }
            }
            Self::Ambiguous { candidates, .. } => {
                format!("write one of the full paths: {}", quoted(candidates))
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
        };

        assert_eq!(error.code(), "resolve/not-found");
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
    fn ambiguous_lists_every_full_path() {
        let error = ResolveError::Ambiguous {
            file: absolute("/repo/src/lib.rs"),
            query: symbol("new"),
            candidates: vec![symbol("Foo.new"), symbol("Bar.new")],
        };

        assert_eq!(error.code(), "resolve/ambiguous");
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

        assert_eq!(error.code(), "read/unsupported-language");
        assert_eq!(
            error.to_string(),
            "no language for `.png` files (/repo/logo.png)"
        );
        assert_eq!(
            error.help(),
            "supdoc reads .md, .rs. A whole-file link to this type can't be tracked yet",
        );
    }
}
