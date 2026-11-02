//! Reading a file into a [`SourceFile`]: the size and UTF-8 checks, then one parse (03 Parsing).

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::limits::{Limits, TimeLimit};
use crate::path::AbsolutePath;
use crate::read::cartridge::{Cartridge, Deadline, Languages, SourceFile};
use crate::read::error::ReadError;

/// The largest file supdoc reads: 16 MiB (03 R13).
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

impl SourceFile {
    /// Parses `text` with `language`'s grammar under the default `parse-timeout`
    /// ([`Limits::PARSE_TIMEOUT`]). Pure apart from reading the clock: the text is already in
    /// memory.
    ///
    /// A file with syntax errors still parses; check [`SourceFile::has_syntax_errors`] (03 R8).
    ///
    /// # Errors
    ///
    /// As [`SourceFile::from_text`].
    pub fn parse(
        path: AbsolutePath,
        text: String,
        language: &'static dyn Cartridge,
    ) -> Result<Self, ReadError> {
        Self::from_text(path, text, language, Deadline::start(Limits::PARSE_TIMEOUT))
    }

    /// Reads `path` from the filesystem and parses it with the cartridge `languages` picks for
    /// the extension of the file it really is.
    ///
    /// A symlink is judged by the file it points at, so `env.py -> .env` is not read as Python.
    ///
    /// # Errors
    ///
    /// - [`ReadError::UnsupportedLanguage`] when no cartridge reads the extension.
    /// - [`ReadError::Missing`] or [`ReadError::Io`] when the file can't be opened or read.
    /// - [`ReadError::TooLarge`] when it is over [`MAX_FILE_BYTES`].
    /// - [`ReadError::NotUtf8`] when it isn't UTF-8.
    /// - [`ReadError::Timeout`] when the parse takes longer than `limit`. The same deadline,
    ///   started just before the parse, then bounds every query and inline parse of the file.
    /// - [`ReadError::Internal`] when tree-sitter returns no tree.
    pub fn read(
        path: AbsolutePath,
        languages: &Languages,
        limit: TimeLimit,
    ) -> Result<Self, ReadError> {
        let real = real_file(&path);
        let Some(language) = languages.for_path(&real) else {
            return Err(unsupported(path, &real, languages));
        };
        let bytes = read_capped(&path)?;
        let text = String::from_utf8(bytes).map_err(|error| ReadError::NotUtf8 {
            path: path.clone(),
            offset: error.utf8_error().valid_up_to(),
        })?;
        Self::from_text(path, text, language, Deadline::start(limit))
    }

    /// Whether the tree has any `ERROR` or missing node. The file is still symbolized and hashed,
    /// but its hashes may change once it parses cleanly (03 R8).
    #[must_use]
    pub fn has_syntax_errors(&self) -> bool {
        self.tree().root_node().has_error()
    }
}

/// Reads the whole file, refusing one over [`MAX_FILE_BYTES`] before reading its contents.
///
/// Only a regular file is opened, after following symlinks: opening a FIFO or a device can block
/// forever, so any other kind is refused before the open.
///
/// The read itself is capped too, so a file that grows after the size check is still refused.
///
/// # Errors
///
/// [`ReadError::Missing`], [`ReadError::Io`] (also for a file that is not a regular file) or
/// [`ReadError::TooLarge`].
fn read_capped(path: &AbsolutePath) -> Result<Vec<u8>, ReadError> {
    let io_error = |error: io::Error| {
        if error.kind() == io::ErrorKind::NotFound {
            ReadError::Missing { path: path.clone() }
        } else {
            ReadError::Io {
                path: path.clone(),
                error,
            }
        }
    };
    let too_large = |size| ReadError::TooLarge {
        path: path.clone(),
        size,
        limit: MAX_FILE_BYTES,
    };

    if !fs::metadata(path.as_path()).map_err(io_error)?.is_file() {
        return Err(io_error(io::Error::new(
            io::ErrorKind::InvalidInput,
            "it is not a regular file",
        )));
    }
    let file = File::open(path.as_path()).map_err(io_error)?;
    let size = file.metadata().map_err(io_error)?.len();
    if size > MAX_FILE_BYTES {
        return Err(too_large(size));
    }
    let mut bytes = Vec::new();
    let _read = file
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    let read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if read > MAX_FILE_BYTES {
        return Err(too_large(read));
    }
    Ok(bytes)
}

/// The file `path` really is once symlinks are followed, or `path` itself when that can't be
/// found (it is missing, and reading it says so).
///
/// Effect: follows symlinks along `path`.
fn real_file(path: &AbsolutePath) -> PathBuf {
    fs::canonicalize(path.as_path()).unwrap_or_else(|_| path.as_path().to_path_buf())
}

/// The error for `path`, whose real file `real` has an extension no cartridge in `languages`
/// reads.
fn unsupported(path: AbsolutePath, real: &Path, languages: &Languages) -> ReadError {
    let extension = real
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut supported: Vec<&'static str> = languages
        .iter()
        .flat_map(|cartridge| cartridge.extensions().iter().copied())
        .collect();
    supported.sort_unstable();
    supported.dedup();
    ReadError::UnsupportedLanguage {
        path,
        extension,
        supported,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::read::error::ReadRole;
    use crate::read::links::links;

    /// Rust text nested deeply enough that its parse and its queries run long past 1 ms.
    fn deep_rust() -> String {
        format!("/// [a](a.md)\nfn add() {{{}\n", "{".repeat(65_536))
    }

    /// A 1 ms deadline that has already passed: waits out the millisecond.
    fn passed_deadline() -> Deadline {
        let deadline = Deadline::start("1ms".parse().expect("1ms should be a time limit"));
        while !deadline.passed() {
            std::hint::spin_loop();
        }
        deadline
    }

    fn rust_path() -> AbsolutePath {
        AbsolutePath::new(Path::new("/repo/src/deep.rs"))
            .expect("the fixture path should be absolute")
    }

    fn rust() -> &'static dyn Cartridge {
        Languages::builtin()
            .for_path(Path::new("deep.rs"))
            .expect("the built-in registry should read `.rs`")
    }

    fn assert_timed_out(error: &ReadError, step: &str) {
        assert_eq!(error.code(), "read-timeout", "{error}");
        let ReadError::Timeout { step: stopped, .. } = error else {
            panic!("expected a timeout, got {error:?}");
        };
        assert_eq!(*stopped, step);
        let help = error.help(ReadRole::Target, "src/deep.rs");
        for knob in [
            "parse-timeout",
            "SUPDOC_PARSE_TIMEOUT",
            "--parse-timeout",
            "stopped at byte",
        ] {
            assert!(help.contains(knob), "the help should name `{knob}`: {help}");
        }
    }

    #[test]
    fn a_passed_deadline_stops_the_parse() {
        let error = SourceFile::from_text(rust_path(), deep_rust(), rust(), passed_deadline())
            .expect_err("the parse should stop at the deadline");

        assert_timed_out(&error, "the parse");
    }

    #[test]
    fn a_passed_deadline_stops_the_symbols_query_with_no_partial_table() {
        let file = SourceFile::parse(rust_path(), deep_rust(), rust())
            .expect("the deep fixture should parse under the default limit")
            .with_deadline(passed_deadline());

        let error = file
            .language()
            .symbols(&file)
            .expect_err("the symbols query should stop at the deadline");

        assert_timed_out(&error, "the symbols query");
    }

    #[test]
    fn a_passed_deadline_stops_reading_links() {
        let file = SourceFile::parse(rust_path(), deep_rust(), rust())
            .expect("the deep fixture should parse under the default limit")
            .with_deadline(passed_deadline());

        let error = links(&file).expect_err("reading links should stop at the deadline");

        assert_timed_out(&error, "the injections query");
    }

    #[test]
    fn a_passed_deadline_stops_an_inline_markdown_parse() {
        let path = AbsolutePath::new(Path::new("/repo/docs/long.md"))
            .expect("the fixture path should be absolute");
        let markdown = Languages::builtin()
            .for_path(Path::new("long.md"))
            .expect("the built-in registry should read `.md`");
        // One long paragraph: the block parse has no query, so the inline parse meets the deadline.
        let text = "[a](b.md) ".repeat(20_000);
        let file = SourceFile::parse(path, text, markdown)
            .expect("the fixture should parse under the default limit")
            .with_deadline(passed_deadline());

        let error = links(&file).expect_err("the inline parse should stop at the deadline");

        assert_timed_out(&error, "an inline Markdown parse");
    }

    #[test]
    fn no_limit_never_stops_a_read() {
        let file = SourceFile::from_text(
            rust_path(),
            "/// [a](a.md)\nfn add() {}\n".to_owned(),
            rust(),
            Deadline::start("0".parse().expect("0 should be a time limit")),
        )
        .expect("a file with no limit should parse");

        assert_eq!(
            file.language()
                .symbols(&file)
                .map(|table| table.iter().count())
                .ok(),
            Some(1)
        );
        assert_eq!(links(&file).map(|found| found.len()).ok(), Some(1));
    }

    fn absolute_in(dir: &tempfile::TempDir, name: &str) -> AbsolutePath {
        AbsolutePath::new(&dir.path().join(name)).expect("a temp dir path should be absolute")
    }

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temp dir should be creatable")
    }

    #[test]
    fn read_parses_a_utf8_file_with_the_cartridge_for_its_extension() {
        let dir = temp_dir();
        let path = absolute_in(&dir, "lib.rs");
        fs::write(path.as_path(), "fn add() {}\n").expect("the fixture should be writable");

        let file = SourceFile::read(path.clone(), &Languages::builtin(), Limits::PARSE_TIMEOUT)
            .expect("a small UTF-8 Rust file should read");

        assert_eq!(file.path(), &path);
        assert_eq!(file.text(), "fn add() {}\n");
        assert_eq!(file.language().id().as_str(), "rust");
        assert!(!file.has_syntax_errors(), "the fixture is valid Rust");
    }

    #[test]
    fn non_utf8_file_is_rejected_with_offset() {
        let dir = temp_dir();
        let path = absolute_in(&dir, "lib.rs");
        fs::write(path.as_path(), b"fn a() {}\n// \xff\n").expect("the fixture should be writable");

        let error = SourceFile::read(path.clone(), &Languages::builtin(), Limits::PARSE_TIMEOUT)
            .expect_err("a file with an invalid UTF-8 byte should be rejected");

        assert!(
            matches!(&error, ReadError::NotUtf8 { offset: 13, .. }),
            "the 0xff byte sits at offset 13, got {error:?}",
        );
        assert_eq!(error.code(), "read-not-utf8");
        assert_eq!(
            error.to_string(),
            format!(
                "{} is not UTF-8 (first invalid byte at offset 13)",
                path.display()
            ),
        );
    }

    #[test]
    fn oversized_file_is_rejected() {
        let dir = temp_dir();
        let path = absolute_in(&dir, "generated.rs");
        let file = File::create(path.as_path()).expect("the fixture should be creatable");
        // Sparse on most filesystems, so the test doesn't write 16 MiB.
        file.set_len(MAX_FILE_BYTES + 1)
            .expect("the fixture should be resizable");

        let error = SourceFile::read(path, &Languages::builtin(), Limits::PARSE_TIMEOUT)
            .expect_err("a file one byte over the limit should be rejected");

        assert!(
            matches!(
                error,
                ReadError::TooLarge {
                    size,
                    limit: MAX_FILE_BYTES,
                    ..
                } if size == MAX_FILE_BYTES + 1
            ),
            "expected TooLarge with the file's size and the 16 MiB limit, got {error:?}",
        );
    }

    #[test]
    fn missing_file_is_reported_as_missing() {
        let dir = temp_dir();
        let path = absolute_in(&dir, "gone.rs");

        let error = SourceFile::read(path, &Languages::builtin(), Limits::PARSE_TIMEOUT)
            .expect_err("a file that doesn't exist can't be read");

        assert_eq!(error.code(), "read-missing", "got {error:?}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_is_read_by_the_extension_of_the_file_it_points_at() {
        let dir = temp_dir();
        fs::write(dir.path().join(".env"), "DB_PASSWORD=hunter2\n")
            .expect("the fixture should be writable");
        let link = absolute_in(&dir, "env.py");
        std::os::unix::fs::symlink(dir.path().join(".env"), link.as_path())
            .expect("the symlink should be creatable");

        let error = SourceFile::read(link, &Languages::builtin(), Limits::PARSE_TIMEOUT)
            .expect_err("a .env file behind a .py name must not be parsed as Python");

        assert_eq!(error.code(), "read-unsupported-language", "got {error:?}");
        assert!(
            !error.to_string().contains("hunter2"),
            "the error must not show the file's contents: {error}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_a_file_with_a_cartridge_extension_is_read_by_it() {
        let dir = temp_dir();
        fs::write(dir.path().join("real.rs"), "fn add() {}\n")
            .expect("the fixture should be writable");
        let link = absolute_in(&dir, "alias.txt");
        std::os::unix::fs::symlink(dir.path().join("real.rs"), link.as_path())
            .expect("the symlink should be creatable");

        let file = SourceFile::read(link.clone(), &Languages::builtin(), Limits::PARSE_TIMEOUT)
            .expect("a symlink to a Rust file should read as Rust");

        assert_eq!(
            file.path(),
            &link,
            "the file keeps the path it was asked for"
        );
        assert_eq!(file.language().id().as_str(), "rust");
    }

    #[test]
    fn missing_png_is_unsupported_not_missing() {
        let dir = temp_dir();
        let path = absolute_in(&dir, "logo.png");

        let error = SourceFile::read(path, &Languages::builtin(), Limits::PARSE_TIMEOUT)
            .expect_err("no cartridge reads .png files");

        assert!(
            matches!(
                &error,
                ReadError::UnsupportedLanguage { extension, supported, .. }
                    if extension == "png" && supported.contains(&"rs")
            ),
            "the missing .png file should be reported as unsupported, not missing: {error:?}",
        );
    }
}
