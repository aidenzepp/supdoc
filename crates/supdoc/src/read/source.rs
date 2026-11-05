//! Reading a file into a [`SourceFile`]: the size and UTF-8 checks, then one parse.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::cartridges::data::KeyPathBudget;
#[cfg(test)]
use crate::limits::Limits;
use crate::limits::{SizeLimit, TimeLimit};
use crate::path::AbsolutePath;
#[cfg(test)]
use crate::read::cartridge::Cartridge;
use crate::read::cartridge::{Deadline, Languages, SourceFile};
use crate::read::error::ReadError;

impl SourceFile {
    /// Parses `text` with `language`'s grammar under the default `parse-timeout`
    /// ([`Limits::PARSE_TIMEOUT`]). Pure apart from reading the clock: the text is already in
    /// memory.
    ///
    /// A file with syntax errors still parses; check [`SourceFile::has_syntax_errors`].
    ///
    /// # Errors
    ///
    /// As [`SourceFile::from_text`].
    #[cfg(test)]
    pub(crate) fn parse(
        path: AbsolutePath,
        text: String,
        language: &'static dyn Cartridge,
    ) -> Result<Self, ReadError> {
        Self::from_text(path, text, language, Deadline::start(Limits::PARSE_TIMEOUT))
    }

    /// Reads `path` from the filesystem and parses it with the cartridge `languages` picks for
    /// the extension of the file it really is, refusing one larger than `max_size`, the run's
    /// `max-file-size`. A data cartridge builds its key paths under `key_paths`, the run's
    /// `max-key-segments` and `max-key-path-size`.
    ///
    /// A symlink is judged by the file it points at, so `env.py -> .env` is not read as Python.
    ///
    /// # Errors
    ///
    /// - [`ReadError::UnsupportedLanguage`] when no cartridge reads the extension.
    /// - [`ReadError::Missing`] or [`ReadError::Io`] when the file can't be opened or read.
    /// - [`ReadError::NotAFile`] when it is a directory, FIFO, socket or device.
    /// - [`ReadError::TooLarge`] when it is over `max_size`.
    /// - [`ReadError::NotUtf8`] when it isn't UTF-8.
    /// - [`ReadError::Timeout`] when the parse takes longer than `limit`. The same deadline,
    ///   started just before the parse, then bounds every query and inline parse of the file.
    /// - [`ReadError::Internal`] when tree-sitter returns no tree.
    pub(crate) fn read(
        path: AbsolutePath,
        languages: &Languages,
        limit: TimeLimit,
        max_size: SizeLimit,
        key_paths: KeyPathBudget,
    ) -> Result<Self, ReadError> {
        let real = real_file(&path);
        let Some(language) = languages.for_path(&real) else {
            return Err(unsupported(path, &real, languages));
        };
        let bytes = read_capped(&path, max_size)?;
        let text = String::from_utf8(bytes).map_err(|error| ReadError::NotUtf8 {
            path: path.clone(),
            offset: error.utf8_error().valid_up_to(),
        })?;
        Self::from_text_under(path, text, language, Deadline::start(limit), key_paths)
    }

    /// Whether the tree has any `ERROR` or missing node. The file is still symbolized and hashed,
    /// but its hashes may change once it parses cleanly.
    #[must_use]
    pub(crate) fn has_syntax_errors(&self) -> bool {
        self.tree().root_node().has_error()
    }
}

/// Reads the whole file, refusing one over `max_size` before reading its contents. With no limit,
/// the whole file is read.
///
/// The file is opened first and judged by the open handle, so what is checked is what is read:
/// no rename between a check and the open can swap in another file. On Unix the open is
/// non-blocking, so a FIFO with no writer can't hang it; anything but a regular file, after
/// following symlinks, is then refused before a byte is read. Some kinds, such as a socket, can't
/// be opened at all: when the open fails, the path itself is looked at once, and one that isn't a
/// regular file is refused the same way.
///
/// The read itself is capped too, so a file that grows after the size check is still refused.
///
/// # Errors
///
/// [`ReadError::Missing`], [`ReadError::NotAFile`], [`ReadError::Io`] or
/// [`ReadError::TooLarge`].
fn read_capped(path: &AbsolutePath, max_size: SizeLimit) -> Result<Vec<u8>, ReadError> {
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
    let too_large = |size, limit| ReadError::TooLarge {
        path: path.clone(),
        size,
        limit,
    };

    let file = open_without_blocking(path.as_path()).map_err(|error| {
        match fs::metadata(path.as_path()) {
            Ok(metadata) if !metadata.is_file() => ReadError::NotAFile { path: path.clone() },
            _ => io_error(error),
        }
    })?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() {
        return Err(ReadError::NotAFile { path: path.clone() });
    }
    let size = metadata.len();
    if let Some(limit) = max_size.get()
        && size > limit
    {
        return Err(too_large(size, limit));
    }
    let bytes = read_up_to(file, max_size).map_err(io_error)?;
    let read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    match max_size.get() {
        Some(limit) if read > limit => Err(too_large(read, limit)),
        _ => Ok(bytes),
    }
}

/// Reads `reader` to its end, but never more than one byte past `max_size`: one byte over is
/// enough to know the source is too large, so a file that keeps growing can't make the read go on.
/// With no limit, it reads to the end.
///
/// # Errors
///
/// Whatever reading `reader` reports.
fn read_up_to(mut reader: impl Read, max_size: SizeLimit) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let _read = match max_size.get() {
        Some(limit) => reader
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)?,
        None => reader.read_to_end(&mut bytes)?,
    };
    Ok(bytes)
}

/// Opens `path` for reading without waiting for a FIFO's writer: on Unix the open is
/// `O_NONBLOCK`, which leaves a regular file's reads blocking as usual.
///
/// Effect: opens a file handle.
///
/// # Errors
///
/// Whatever the open reports, such as `NotFound` or `PermissionDenied`.
#[cfg(unix)]
pub(crate) fn open_without_blocking(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

/// Opens `path` for reading. Only Unix has FIFOs that block an open.
///
/// Effect: opens a file handle.
///
/// # Errors
///
/// Whatever the open reports, such as `NotFound` or `PermissionDenied`.
#[cfg(not(unix))]
pub(crate) fn open_without_blocking(path: &Path) -> io::Result<File> {
    File::open(path)
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

    /// The deep fixture, parsed with no limit so no clock decides whether it parses.
    fn parsed_deep_rust() -> SourceFile {
        SourceFile::from_text(
            rust_path(),
            deep_rust(),
            rust(),
            Deadline::start("0".parse().expect("0 should be a time limit")),
        )
        .expect("the deep fixture should parse with no limit")
    }

    fn rust_path() -> AbsolutePath {
        AbsolutePath::new(Path::new("/repo/src/deep.rs"))
            .expect("the fixture path should be absolute")
    }

    fn rust() -> &'static dyn Cartridge {
        Languages::builtin()
            .expect("the built-in cartridges should load")
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
        let file = parsed_deep_rust().with_deadline(passed_deadline());

        let error = file
            .language()
            .symbols(&file)
            .expect_err("the symbols query should stop at the deadline");

        assert_timed_out(&error, "the symbols query");
    }

    #[test]
    fn a_passed_deadline_stops_reading_links() {
        let file = parsed_deep_rust().with_deadline(passed_deadline());

        let error = links(&file).expect_err("reading links should stop at the deadline");

        assert_timed_out(&error, "the injections query");
    }

    #[test]
    fn a_passed_deadline_stops_an_inline_markdown_parse() {
        let path = AbsolutePath::new(Path::new("/repo/docs/long.md"))
            .expect("the fixture path should be absolute");
        let markdown = Languages::builtin()
            .expect("the built-in cartridges should load")
            .for_path(Path::new("long.md"))
            .expect("the built-in registry should read `.md`");
        // One long paragraph: the block parse has no query, so the inline parse meets the deadline.
        let text = "[a](b.md) ".repeat(20_000);
        let file = SourceFile::from_text(
            path,
            text,
            markdown,
            Deadline::start("0".parse().expect("0 should be a time limit")),
        )
        .expect("the fixture should parse with no limit")
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

        let file = SourceFile::read(
            path.clone(),
            Languages::builtin().expect("the built-in cartridges should load"),
            Limits::PARSE_TIMEOUT,
            Limits::MAX_FILE_SIZE,
            KeyPathBudget::defaults(),
        )
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

        let error = SourceFile::read(
            path.clone(),
            Languages::builtin().expect("the built-in cartridges should load"),
            Limits::PARSE_TIMEOUT,
            Limits::MAX_FILE_SIZE,
            KeyPathBudget::defaults(),
        )
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

    /// A sparse `.rs` file of `len` bytes in `dir`, so a large size costs no disk.
    fn sparse_file(dir: &tempfile::TempDir, len: u64) -> AbsolutePath {
        let path = absolute_in(dir, "generated.rs");
        let file = File::create(path.as_path()).expect("the fixture should be creatable");
        file.set_len(len).expect("the fixture should be resizable");
        path
    }

    /// Reads `path` under the `max-file-size` ceiling `max_size`.
    fn read_under(path: AbsolutePath, max_size: SizeLimit) -> Result<SourceFile, ReadError> {
        SourceFile::read(
            path,
            Languages::builtin().expect("the built-in cartridges should load"),
            Limits::PARSE_TIMEOUT,
            max_size,
            KeyPathBudget::defaults(),
        )
    }

    #[test]
    fn a_file_one_byte_over_the_default_cap_is_read_too_large() {
        let dir = temp_dir();
        let cap = Limits::MAX_FILE_SIZE
            .get()
            .expect("the default max-file-size should be a limit");
        let path = sparse_file(&dir, cap + 1);

        let error = read_under(path, Limits::MAX_FILE_SIZE)
            .expect_err("a file one byte over the limit should be rejected");

        assert!(
            matches!(
                error,
                ReadError::TooLarge { size, limit, .. } if size == cap + 1 && limit == cap
            ),
            "expected TooLarge with the file's size and the default limit, got {error:?}",
        );
        assert_eq!(error.code(), "read-too-large");
        assert!(
            error.to_string().contains("`max-file-size`"),
            "the message should name the key: {error}"
        );
    }

    #[test]
    fn a_file_of_exactly_the_cap_reads() {
        let dir = temp_dir();
        let path = absolute_in(&dir, "lib.rs");
        fs::write(path.as_path(), "fn add() {}\n").expect("the fixture should be writable");

        let file = read_under(path, SizeLimit::bytes(12))
            .expect("a file exactly at the limit should read");

        assert_eq!(file.text(), "fn add() {}\n");
        let path = absolute_in(&dir, "lib.rs");
        let error = read_under(path, SizeLimit::bytes(11))
            .expect_err("a file one byte over the limit should be rejected");
        assert_eq!(error.code(), "read-too-large");
    }

    /// A reader of `len` bytes that counts how many it has handed out.
    struct Counted {
        /// The bytes still to hand out.
        inner: io::Take<io::Repeat>,
        /// How many bytes it has handed out so far.
        handed_out: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl Read for Counted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buf)?;
            self.handed_out
                .set(self.handed_out.get() + u64::try_from(read).expect("a read fits in u64"));
            Ok(read)
        }
    }

    #[test]
    fn a_capped_read_stops_one_byte_past_the_cap_however_much_the_source_holds() {
        let handed_out = std::rc::Rc::new(std::cell::Cell::new(0));
        let source = Counted {
            inner: io::repeat(b'x').take(4096),
            handed_out: std::rc::Rc::clone(&handed_out),
        };

        let bytes = read_up_to(source, SizeLimit::bytes(100)).expect("an in-memory read succeeds");

        assert_eq!(bytes.len(), 101, "one byte past the cap is kept");
        assert_eq!(
            handed_out.get(),
            101,
            "the read should stop at cap + 1 bytes, not read the whole source first"
        );
    }

    #[test]
    fn zero_lifts_the_cap_so_a_file_past_the_default_is_read_whole() {
        let dir = temp_dir();
        let cap = Limits::MAX_FILE_SIZE
            .get()
            .expect("the default max-file-size should be a limit");
        let path = sparse_file(&dir, cap + 1);

        // The bytes alone: parsing a file this size would cost far more memory than reading it.
        let bytes = read_capped(&path, SizeLimit::bytes(0)).expect("with no limit, the file reads");

        assert_eq!(u64::try_from(bytes.len()).ok(), Some(cap + 1));
    }

    #[test]
    fn missing_file_is_reported_as_missing() {
        let dir = temp_dir();
        let path = absolute_in(&dir, "gone.rs");

        let error = SourceFile::read(
            path,
            Languages::builtin().expect("the built-in cartridges should load"),
            Limits::PARSE_TIMEOUT,
            Limits::MAX_FILE_SIZE,
            KeyPathBudget::defaults(),
        )
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

        let error = SourceFile::read(
            link,
            Languages::builtin().expect("the built-in cartridges should load"),
            Limits::PARSE_TIMEOUT,
            Limits::MAX_FILE_SIZE,
            KeyPathBudget::defaults(),
        )
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

        let file = SourceFile::read(
            link.clone(),
            Languages::builtin().expect("the built-in cartridges should load"),
            Limits::PARSE_TIMEOUT,
            Limits::MAX_FILE_SIZE,
            KeyPathBudget::defaults(),
        )
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

        let error = SourceFile::read(
            path,
            Languages::builtin().expect("the built-in cartridges should load"),
            Limits::PARSE_TIMEOUT,
            Limits::MAX_FILE_SIZE,
            KeyPathBudget::defaults(),
        )
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

    /// `SourceFile::read(path)`, run on its own thread so an open that blocks fails the test
    /// after five seconds instead of hanging it.
    fn read_within_five_seconds(path: AbsolutePath) -> Result<SourceFile, ReadError> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let _reader = std::thread::spawn(move || {
            let read = SourceFile::read(
                path,
                Languages::builtin().expect("the built-in cartridges should load"),
                Limits::PARSE_TIMEOUT,
                Limits::MAX_FILE_SIZE,
                KeyPathBudget::defaults(),
            );
            let _sent = sender.send(read);
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reading should return within five seconds, not block on the open")
    }

    #[cfg(unix)]
    #[test]
    fn fifo_target_is_not_a_file_and_never_blocks() {
        let dir = temp_dir();
        let path = absolute_in(&dir, "pipe.rs");
        let made = std::process::Command::new("mkfifo")
            .arg(path.as_path())
            .status()
            .expect("mkfifo should run");
        assert!(made.success(), "mkfifo should create the FIFO");

        let error = read_within_five_seconds(path)
            .expect_err("a FIFO with no writer is not a file supdoc reads");

        assert!(
            matches!(error, ReadError::NotAFile { .. }),
            "a FIFO should be refused as not a regular file: {error:?}"
        );
        assert_eq!(error.code(), "read-not-a-file");
    }

    #[cfg(unix)]
    #[test]
    fn socket_target_is_not_a_file() {
        let dir = temp_dir();
        let path = absolute_in(&dir, "x.rs");
        let _listener = std::os::unix::net::UnixListener::bind(path.as_path())
            .expect("the socket should bind in the temp dir");

        let error = read_within_five_seconds(path)
            .expect_err("a socket, which can't be opened, is not a file supdoc reads");

        assert!(
            matches!(error, ReadError::NotAFile { .. }),
            "a socket should be refused as not a regular file, not as an I/O error: {error:?}"
        );
        assert_eq!(error.code(), "read-not-a-file");
    }

    #[cfg(unix)]
    #[test]
    fn directory_target_is_not_a_file() {
        let dir = temp_dir();
        let path = absolute_in(&dir, "folder.rs");
        fs::create_dir(path.as_path()).expect("the directory should be creatable");

        let error =
            read_within_five_seconds(path).expect_err("a directory is not a file supdoc reads");

        assert!(
            matches!(error, ReadError::NotAFile { .. }),
            "a directory should be refused as not a regular file: {error:?}"
        );
        let help = error.help(ReadRole::Target, "folder.rs");
        assert!(
            help.contains("regular file") && !help.contains("chmod"),
            "the help should ask for a regular file, not a permission change: {help}"
        );
    }
}
