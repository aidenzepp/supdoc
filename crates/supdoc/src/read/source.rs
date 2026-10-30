//! Reading a file into a [`SourceFile`]: the size and UTF-8 checks, then one parse (03 Parsing).

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::path::AbsolutePath;
use crate::read::cartridge::{Cartridge, Languages, SourceFile};
use crate::read::error::ReadError;

/// The largest file supdoc reads: 16 MiB (03 R13).
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

impl SourceFile {
    /// Parses `text` with `language`'s grammar. Pure: the text is already in memory.
    ///
    /// A file with syntax errors still parses; check [`SourceFile::has_syntax_errors`] (03 R8).
    ///
    /// # Errors
    ///
    /// [`ReadError::Internal`] if tree-sitter returns no tree. supdoc sets no timeout or
    /// cancellation, so that is a supdoc bug.
    pub fn parse(
        path: AbsolutePath,
        text: String,
        language: &'static dyn Cartridge,
    ) -> Result<Self, ReadError> {
        let internal = ReadError::Internal { path: path.clone() };
        Self::from_text(path, text, language).ok_or(internal)
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
    /// - [`ReadError::Internal`] when tree-sitter returns no tree.
    pub fn read(path: AbsolutePath, languages: &Languages) -> Result<Self, ReadError> {
        let real = real_file(&path);
        let Some(language) = languages.for_path(&real) else {
            return Err(unsupported(path, &real, languages));
        };
        let bytes = read_capped(&path)?;
        let text = String::from_utf8(bytes).map_err(|error| ReadError::NotUtf8 {
            path: path.clone(),
            offset: error.utf8_error().valid_up_to(),
        })?;
        Self::parse(path, text, language)
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
/// The read itself is capped too, so a file that grows after the size check is still refused.
///
/// # Errors
///
/// [`ReadError::Missing`], [`ReadError::Io`] or [`ReadError::TooLarge`].
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

        let file = SourceFile::read(path.clone(), &Languages::builtin())
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

        let error = SourceFile::read(path.clone(), &Languages::builtin())
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

        let error = SourceFile::read(path, &Languages::builtin())
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

        let error = SourceFile::read(path, &Languages::builtin())
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

        let error = SourceFile::read(link, &Languages::builtin())
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

        let file = SourceFile::read(link.clone(), &Languages::builtin())
            .expect("a symlink to a Rust file should read as Rust");

        assert_eq!(
            file.path(),
            &link,
            "the file keeps the path it was asked for"
        );
        assert_eq!(file.language().id().as_str(), "rust");
    }

    #[test]
    fn unknown_extension_is_unsupported_without_touching_the_filesystem() {
        let dir = temp_dir();
        let path = absolute_in(&dir, "logo.png");

        let error = SourceFile::read(path, &Languages::builtin())
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
