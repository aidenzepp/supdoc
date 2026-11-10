//! The result cache for tool runs: a formatter's exit, stdout and capped stderr for one block,
//! kept outside the checkout so an unchanged block isn't run again.
//!
//! Entries live in `$XDG_CACHE_HOME/supdoc/code-blocks/<sha256 of the real root>/`, or under
//! `$HOME/.cache` (on Windows, `%LOCALAPPDATA%`) when `XDG_CACHE_HOME` isn't an absolute path free
//! of `..`. A missing, unreadable, corrupt or
//! partial entry is a miss, never a pass.

use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

use crate::config::confine::{self, Admitted, Boundary};
use crate::hash::Hash;
use crate::limits::{Limits, SizeLimit};
use crate::path::{self, Component, fixed};
use crate::read::source::{open_without_blocking, read_up_to};

use super::runner::{self, Ran};
use super::tools::{ToolCartridge, ToolEnv};

/// The version of the rules that decide what a tool reads for a block, such as wrapping. Raised
/// whenever they change, so results judged under old rules are never reused.
const RULES_VERSION: u32 = 3;

/// The first bytes of every entry, naming its format.
const MAGIC: &[u8] = b"supdoc code-block cache 1\n";

/// The most bytes of a tool's stderr an entry holds; a run with more is not stored.
const STDERR_CAP: usize = 64 * 1024;

/// The largest entry read back: the runner's 1 MiB output cap, the stderr cap and the framing.
const MAX_ENTRY: u64 = 1024 * 1024 + 64 * 1024 + 4096;

/// How many entries one root's cache keeps; the oldest beyond it are removed.
const MAX_ENTRIES: usize = 4096;

/// Tells concurrent writers' temporary files apart within one process.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// One root's cache directory.
#[derive(Debug)]
pub(crate) struct Cache {
    /// The directory holding the root's entries.
    dir: PathBuf,
    /// `dir`, where it really lay when the cache opened: every create, write, rename and remove
    /// is admitted against it.
    boundary: Boundary,
    /// How many entries it keeps.
    limit: usize,
}

/// The key of one tool run: a sha256 over everything that decides its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Key(Hash);

/// What decides one tool run's result, hashed into its [`Key`].
#[derive(Debug)]
pub(crate) struct Run<'run> {
    /// The tool.
    pub(crate) tool: &'run ToolCartridge,
    /// The tool's real path.
    pub(crate) program: &'run Path,
    /// Its arguments.
    pub(crate) args: &'run [&'run OsStr],
    /// The variables it gets beyond the passed environment, such as the pinned toolchain.
    pub(crate) extra: &'run [(&'run str, &'run OsStr)],
    /// The text the tool reads.
    pub(crate) input: &'run str,
    /// The doc's absolute path, the virtual path's directory the tool's config search starts from.
    pub(crate) doc: &'run Path,
    /// The fingerprint of the project build the block is linked against, `None` when it compiles
    /// alone: the externs' paths don't change when their bytes do.
    pub(crate) project: Option<&'run [u8; 32]>,
}

impl Cache {
    /// The cache for `root`, under the first of `env`'s [`runner::cache_bases`], with its
    /// directory created mode 0700 beneath where that base really lies, as
    /// [`runner::create_private`] creates the other private directories. `None` when there is no
    /// base (each unset, not absolute or holding a `..`), when the directory would be inside
    /// `root`, when a link beneath the base leads out of it, or when it can't be created: then
    /// nothing is cached.
    ///
    /// Effect: resolves `root` and the cache base, and creates the cache directory.
    pub(crate) fn open(env: &ToolEnv, root: &Path) -> Option<Self> {
        let base = runner::cache_bases(env).next()?;
        let real_root = root.canonicalize().ok()?;
        let real_base = resolved(&base)?;
        let name =
            Component::new(&Hash::digest(real_root.as_os_str().as_encoded_bytes()).to_string())?;
        let dir = path::beneath(
            &path::beneath(
                &path::beneath(&real_base, &fixed::SUPDOC),
                &fixed::CODE_BLOCKS,
            ),
            &name,
        );
        let existing = dir
            .ancestors()
            .find_map(|ancestor| ancestor.canonicalize().ok());
        if existing.is_none_or(|existing| existing.starts_with(&real_root)) {
            return None;
        }
        Self::at(&real_base, dir, MAX_ENTRIES)
    }

    /// The cache in `dir`, keeping `limit` entries, once [`runner::create_private`] creates it
    /// mode 0700 admitted beneath `base`; later writes are held beneath where `dir` really lies
    /// then. `None` when either isn't an exact absolute path, when `dir` leaves `base` once its
    /// links are followed, or when it can't be created.
    ///
    /// Effect: resolves `base`, and creates `dir` and its missing parents.
    fn at(base: &Path, dir: PathBuf, limit: usize) -> Option<Self> {
        let (base, given) = (runner::exact(base)?, runner::exact(&dir)?);
        let boundary = runner::create_private(&Boundary::resolve(&base), &given)?;
        Some(Self {
            dir,
            boundary,
            limit,
        })
    }

    /// The key of `run`, from `root`: the checked text, the tool's name, its real path, size and
    /// modification time (read, never run), its arguments and extra variables, each config file
    /// its row names along the tool's own search (see [`config_state`]), the project build's
    /// fingerprint when there is one, and [`RULES_VERSION`].
    /// `None` when the tool's metadata or an in-root config file can't be read, so the run isn't
    /// cached.
    ///
    /// Effect: reads the tool's metadata, each in-root config candidate, and the metadata of each
    /// candidate above `root`.
    pub(crate) fn key(run: &Run<'_>, root: &Path) -> Option<Key> {
        let meta = fs::metadata(run.program).ok()?;
        let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(&RULES_VERSION.to_le_bytes());
        field(run.tool.name.as_bytes());
        field(run.program.as_os_str().as_encoded_bytes());
        field(&meta.len().to_le_bytes());
        field(&modified.as_nanos().to_le_bytes());
        for arg in run.args {
            field(arg.as_encoded_bytes());
        }
        for (name, value) in run.extra {
            field(name.as_bytes());
            field(value.as_encoded_bytes());
        }
        field(run.project.map_or(&[][..], |fingerprint| &fingerprint[..]));
        let real_root = root.canonicalize().ok()?;
        for dir in run.doc.parent().into_iter().flat_map(Path::ancestors) {
            for name in run.tool.config_files {
                let candidate = crate::path::beneath(dir, name);
                field(candidate.as_os_str().as_encoded_bytes());
                field(&config_state(
                    &candidate,
                    dir.starts_with(root),
                    &real_root,
                )?);
            }
        }
        field(run.input.as_bytes());
        Some(Key(Hash::from_bytes(hasher.finalize().into())))
    }

    /// The run stored under `key`. `None` when there is no entry or it can't be read, is cut
    /// short, is corrupt or belongs to another key.
    ///
    /// Effect: reads the entry.
    pub(crate) fn get(&self, key: Key) -> Option<Ran> {
        let path = self.entry(key)?;
        if fs::metadata(&path).ok()?.len() > MAX_ENTRY {
            return None;
        }
        decode(&fs::read(path).ok()?, key)
    }

    /// Stores `ran` under `key` through a temporary file renamed into place. A run whose stderr
    /// is longer than `STDERR_CAP` is not stored: a cut stderr could hide the diagnostic a later
    /// run needs. A failed write leaves no entry and is otherwise ignored.
    ///
    /// Effect: writes and renames a file in the cache directory.
    pub(crate) fn put(&self, key: Key, ran: &Ran) {
        if ran.stderr.len() > STDERR_CAP {
            return;
        }
        let count = WRITES.fetch_add(1, Ordering::Relaxed);
        let Some(name) = Component::new(&format!(".tmp-{}-{count}", std::process::id())) else {
            return;
        };
        let temporary = path::beneath(&self.dir, &name);
        let Some(created) = self.admit(&temporary) else {
            return;
        };
        let Ok(mut file) = confine::create_new(&created, 0o666) else {
            return;
        };
        let stored = file.write_all(&encode(key, ran)).is_ok()
            && self
                .admit(&temporary)
                .zip(self.entry(key).and_then(|entry| self.admit(&entry)))
                .is_some_and(|(from, to)| confine::rename_into(&from, &to).is_ok());
        if !stored && let Some(temporary) = self.admit(&temporary) {
            let _gone = confine::remove_file(&temporary);
        }
    }

    /// Removes the oldest entries beyond the limit, by modification time. Failures are ignored.
    ///
    /// Effect: lists the cache directory and removes files from it.
    pub(crate) fn prune(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut files: Vec<_> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let meta = entry.metadata().ok()?;
                meta.is_file()
                    .then(|| (meta.modified().unwrap_or(UNIX_EPOCH), entry.path()))
            })
            .collect();
        let Some(excess) = files.len().checked_sub(self.limit) else {
            return;
        };
        files.sort();
        for (_, path) in files.into_iter().take(excess) {
            if let Some(entry) = self.admit(&path) {
                let _gone = confine::remove_file(&entry);
            }
        }
    }

    /// The path of `key`'s entry, or `None` when its name is not one path component.
    fn entry(&self, key: Key) -> Option<PathBuf> {
        Some(path::beneath(
            &self.dir,
            &Component::new(&key.0.to_string())?,
        ))
    }

    /// `path`, once [`Boundary::admit`] finds it inside the cache directory both as written and
    /// once every symlink along it is followed; `None` when it doesn't. Call it immediately before
    /// the write it guards.
    ///
    /// Effect: reads file metadata and symlinks along `path`.
    fn admit(&self, path: &Path) -> Option<Admitted<'_>> {
        self.boundary.admit(path).ok()
    }
}

/// What a config candidate adds to a key: `absent`, the sha256 of its content when it is
/// `in_root` and its real path is inside `real_root`, else only its existence, size and
/// modification time: a file outside the root is never read. `None` when an in-root file exists
/// but can't be hashed ([`content_digest`]).
///
/// Effect: reads `candidate`'s metadata, and its content when it is inside the root.
fn config_state(candidate: &Path, in_root: bool, real_root: &Path) -> Option<Vec<u8>> {
    let Ok(meta) = fs::metadata(candidate) else {
        return Some(b"absent".to_vec());
    };
    let contained = in_root
        && candidate
            .canonicalize()
            .is_ok_and(|real| real.starts_with(real_root));
    if contained {
        return content_digest(candidate);
    }
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos());
    let mut state = b"outside".to_vec();
    state.extend_from_slice(&meta.len().to_le_bytes());
    state.extend_from_slice(&modified.to_le_bytes());
    Some(state)
}

/// The sha256 of the regular file at `path`. `None` when it can't be opened or read, isn't a
/// regular file, or holds more than [`Limits::MAX_FILE_SIZE`] bytes ([`digest_of`]): the block
/// then runs uncached. The fixed ceiling, not the operator's `max-file-size`, bounds it, since going over
/// only costs caching and `0` there must not make the hash unbounded. The open never waits on a
/// FIFO, and the read never takes more than one byte past the ceiling.
///
/// Effect: opens `path` and reads at most one byte past the ceiling of it.
fn content_digest(path: &Path) -> Option<Vec<u8>> {
    let file = open_without_blocking(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    digest_of(file, meta.len(), Limits::MAX_FILE_SIZE)
}

/// The sha256 of what `reader` holds, read through [`read_up_to`]. `None` when `len`, its declared
/// size, or the bytes it actually hands out pass `max_size`, when `max_size` is no limit, or when
/// the read fails. An oversize `len` is refused before any byte is read, and the read never takes
/// more than one byte past `max_size`, so a source that grows can't make it go on.
///
/// Effect: reads at most one byte past `max_size` of `reader`.
fn digest_of(reader: impl Read, len: u64, max_size: SizeLimit) -> Option<Vec<u8>> {
    let limit = max_size.get()?;
    if len > limit {
        return None;
    }
    let bytes = read_up_to(reader, max_size).ok()?;
    let read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    (read <= limit).then(|| Sha256::digest(&bytes).to_vec())
}

/// `path` with its longest existing ancestor resolved and the rest joined on. `None` when no
/// ancestor resolves.
///
/// Effect: reads metadata and resolves symlinks along `path`.
fn resolved(path: &Path) -> Option<PathBuf> {
    let (existing, real) = path
        .ancestors()
        .find_map(|ancestor| Some((ancestor, ancestor.canonicalize().ok()?)))?;
    if existing == path {
        return Some(real);
    }
    path::rebase(path, existing, &real)
}

/// `ran`'s entry under `key`: the magic, the key, the exit, both stream lengths, the streams,
/// then a sha256 of all of it. `ran.stderr` must not exceed `STDERR_CAP`; `Cache::put` checks.
fn encode(key: Key, ran: &Ran) -> Vec<u8> {
    let stderr = &ran.stderr;
    let mut bytes =
        Vec::with_capacity(MAGIC.len() + 32 + 17 + ran.stdout.len() + stderr.len() + 32);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&key.0.to_string().into_bytes());
    bytes.push(u8::from(ran.success));
    bytes.extend_from_slice(&(ran.stdout.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&(stderr.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&ran.stdout);
    bytes.extend_from_slice(stderr);
    let sum = Sha256::digest(&bytes);
    bytes.extend_from_slice(&sum);
    bytes
}

/// The run in `bytes`, an entry stored under `key`. `None` unless every part is present, the
/// lengths account for every byte and the trailing sha256 matches.
fn decode(bytes: &[u8], key: Key) -> Option<Ran> {
    let (body, sum) = bytes.split_at_checked(bytes.len().checked_sub(32)?)?;
    if Sha256::digest(body).as_slice() != sum {
        return None;
    }
    let rest = body.strip_prefix(MAGIC)?;
    let rest = rest.strip_prefix(key.0.to_string().as_bytes())?;
    let (&success, rest) = rest.split_first()?;
    let (stdout_len, rest) = rest.split_first_chunk::<8>()?;
    let (stderr_len, rest) = rest.split_first_chunk::<8>()?;
    let stdout_len = usize::try_from(u64::from_le_bytes(*stdout_len)).ok()?;
    let stderr_len = usize::try_from(u64::from_le_bytes(*stderr_len)).ok()?;
    let (stdout, stderr) = rest.split_at_checked(stdout_len)?;
    if stderr.len() != stderr_len || success > 1 {
        return None;
    }
    Some(Ran {
        success: success == 1,
        stdout: stdout.to_vec(),
        stderr: stderr.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::code_blocks::tools::CARTRIDGES;
    use crate::read::source::Counted;

    /// A temporary directory for one test.
    fn temp() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("a temporary directory should be creatable")
    }

    /// The `config_state` of `candidate`, a file in `root`, run on a thread so a blocking open
    /// fails the test instead of hanging it.
    fn in_root_state(candidate: &Path, root: &Path) -> Option<Vec<u8>> {
        let real_root = root.canonicalize().expect("the root should canonicalize");
        let (sender, receiver) = mpsc::channel();
        let read = candidate.to_path_buf();
        let _detached =
            thread::spawn(move || drop(sender.send(config_state(&read, true, &real_root))));
        let result = receiver.recv_timeout(Duration::from_secs(3));
        if result.is_err() {
            // Unblock a reader stuck in open, so the thread ends.
            drop(fs::OpenOptions::new().write(true).open(candidate));
        }
        result.expect("reading a config candidate should return, not block")
    }

    #[test]
    fn a_declared_size_over_the_ceiling_is_refused_before_any_byte_is_read() {
        let (source, handed_out) = Counted::new(4096);

        assert_eq!(digest_of(source, 101, SizeLimit::bytes(100)), None);
        assert_eq!(
            handed_out.get(),
            0,
            "an oversize file should not be read at all"
        );
    }

    #[test]
    fn a_source_that_outgrows_its_declared_size_stops_one_byte_past_the_ceiling() {
        let (source, handed_out) = Counted::new(4096);

        assert_eq!(digest_of(source, 0, SizeLimit::bytes(100)), None);
        assert_eq!(
            handed_out.get(),
            101,
            "the hash should read at most the ceiling + 1 bytes, not the whole source"
        );
    }

    #[test]
    fn a_source_at_the_ceiling_is_hashed_whole() {
        let (source, _) = Counted::new(100);

        assert_eq!(
            digest_of(source, 100, SizeLimit::bytes(100)),
            Some(Sha256::digest([b'x'; 100]).to_vec())
        );
    }

    #[test]
    fn no_ceiling_gives_no_digest_rather_than_an_unbounded_read() {
        let (source, handed_out) = Counted::new(4096);

        assert_eq!(digest_of(source, 0, SizeLimit::bytes(0)), None);
        assert_eq!(handed_out.get(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_tool_config_in_the_root_has_no_cache_key() {
        let dir = temp();
        let fifo = dir.path().join("rustfmt.toml");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo should be runnable");
        assert!(made.success(), "mkfifo failed for {}", fifo.display());

        assert_eq!(in_root_state(&fifo, dir.path()), None);
    }

    #[test]
    fn a_tool_config_over_the_file_ceiling_has_no_cache_key() {
        let dir = temp();
        let config = dir.path().join("rustfmt.toml");
        let limit = Limits::MAX_FILE_SIZE
            .get()
            .expect("the file ceiling should be set");
        fs::File::create(&config)
            .and_then(|file| file.set_len(limit + 1))
            .expect("the sparse config should be creatable");

        assert_eq!(in_root_state(&config, dir.path()), None);
    }

    #[test]
    fn a_small_tool_config_keys_by_the_sha256_of_its_content() {
        let dir = temp();
        let config = dir.path().join("rustfmt.toml");
        fs::write(&config, "max_width = 80\n").expect("the config should be writable");
        let whole = fs::read(&config).expect("the config should be readable");

        assert_eq!(
            in_root_state(&config, dir.path()),
            Some(Sha256::digest(&whole).to_vec()),
            "the streamed digest should match hashing the whole file, so existing caches stay valid"
        );
    }

    /// The `shfmt` row.
    fn shfmt() -> &'static ToolCartridge {
        CARTRIDGES
            .iter()
            .find(|tool| tool.name == "shfmt")
            .expect("shfmt should be a known tool")
    }

    /// A program file in `dir` to key runs by.
    fn program(dir: &Path) -> PathBuf {
        let path = dir.join("shfmt");
        fs::write(&path, "#!/bin/sh\n").expect("the program should be writable");
        path
    }

    /// The key of `shfmt` at `program` on `input` from `root`.
    fn key_of(program: &Path, input: &str, root: &Path) -> Key {
        key_against(program, input, root, None)
    }

    /// The key of `shfmt` at `program` on `input` from `root`, linked against the build
    /// `project` fingerprints.
    fn key_against(program: &Path, input: &str, root: &Path, project: Option<&[u8; 32]>) -> Key {
        let doc = root.join("docs/a.md");
        let run = Run {
            tool: shfmt(),
            program,
            args: &[OsStr::new("--filename"), OsStr::new("a.sh")],
            extra: &[],
            input,
            doc: &doc,
            project,
        };
        Cache::key(&run, root).expect("the program's metadata should be readable")
    }

    /// A finished run.
    fn ran() -> Ran {
        Ran {
            success: true,
            stdout: b"echo hi\n".to_vec(),
            stderr: b"note\n".to_vec(),
        }
    }

    #[test]
    fn a_stored_run_reads_back_unchanged() {
        let dir = temp();
        let cache =
            Cache::at(dir.path(), dir.path().join("cache"), 10).expect("the cache should open");
        let key = key_of(&program(dir.path()), "echo hi\n", dir.path());

        cache.put(key, &ran());

        assert_eq!(cache.get(key), Some(ran()));
    }

    #[test]
    fn a_missing_entry_is_a_miss() {
        let dir = temp();
        let cache =
            Cache::at(dir.path(), dir.path().join("cache"), 10).expect("the cache should open");
        let key = key_of(&program(dir.path()), "echo hi\n", dir.path());

        assert_eq!(cache.get(key), None);
    }

    #[test]
    fn a_corrupt_or_cut_short_entry_is_a_miss() {
        let dir = temp();
        let cache =
            Cache::at(dir.path(), dir.path().join("cache"), 10).expect("the cache should open");
        let key = key_of(&program(dir.path()), "echo hi\n", dir.path());
        cache.put(key, &ran());
        let path = cache.entry(key).expect("a key names one component");
        let whole = fs::read(&path).expect("the entry should be readable");

        let mut flipped = whole.clone();
        if let Some(byte) = flipped.get_mut(MAGIC.len() + 70) {
            *byte ^= 1;
        }
        fs::write(&path, &flipped).expect("the entry should be writable");
        let corrupt = cache.get(key);
        fs::write(&path, whole.get(..whole.len() - 1).unwrap_or_default())
            .expect("the entry should be writable");
        let short = cache.get(key);

        assert_eq!((corrupt, short), (None, None));
    }

    #[test]
    fn an_entry_under_another_key_is_a_miss() {
        let dir = temp();
        let cache =
            Cache::at(dir.path(), dir.path().join("cache"), 10).expect("the cache should open");
        let program = program(dir.path());
        let first = key_of(&program, "echo hi\n", dir.path());
        let second = key_of(&program, "echo bye\n", dir.path());
        cache.put(first, &ran());

        fs::rename(
            cache.entry(first).expect("a key names one component"),
            cache.entry(second).expect("a key names one component"),
        )
        .expect("the entry should move");

        assert_eq!(cache.get(second), None);
    }

    #[test]
    fn the_key_changes_with_the_text_the_config_the_tool_and_the_project_build() {
        let dir = temp();
        let program = program(dir.path());
        let base = key_of(&program, "echo hi\n", dir.path());

        let text = key_of(&program, "echo bye\n", dir.path());
        fs::write(dir.path().join(".editorconfig"), "[*]\n").expect("config should be writable");
        let config = key_of(&program, "echo hi\n", dir.path());
        fs::write(&program, "#!/bin/sh\nexit 0\n").expect("the program should be writable");
        let tool = key_of(&program, "echo hi\n", dir.path());

        assert_ne!(base, text, "the text");
        assert_ne!(base, config, "the config file");
        assert_ne!(config, tool, "the tool's binary");
        let built = key_against(&program, "echo hi\n", dir.path(), Some(&[1; 32]));
        let rebuilt = key_against(&program, "echo hi\n", dir.path(), Some(&[2; 32]));
        assert_ne!(tool, built, "a project build");
        assert_ne!(built, rebuilt, "the project build's fingerprint");
        assert_eq!(
            tool,
            key_of(&program, "echo hi\n", dir.path()),
            "same inputs"
        );
    }

    #[test]
    fn the_key_changes_when_a_config_appears_in_the_docs_directory_or_above_the_root() {
        let dir = temp();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("docs")).expect("the docs directory should be creatable");
        let program = program(dir.path());
        let base = key_of(&program, "echo hi\n", &root);

        fs::write(root.join("docs/.editorconfig"), "[*]\n").expect("config should be writable");
        let nested = key_of(&program, "echo hi\n", &root);
        fs::write(dir.path().join(".editorconfig"), "[*]\n").expect("config should be writable");
        let above = key_of(&program, "echo hi\n", &root);

        assert_ne!(base, nested, "a config in the doc's directory");
        assert_ne!(nested, above, "a config above the root");
    }

    /// A failed run whose stderr is `len` bytes.
    fn loud(len: usize) -> Ran {
        Ran {
            success: false,
            stdout: Vec::new(),
            stderr: vec![b'e'; len],
        }
    }

    #[test]
    fn stderr_at_the_cap_is_stored_whole() {
        let dir = temp();
        let cache =
            Cache::at(dir.path(), dir.path().join("cache"), 10).expect("the cache should open");
        let key = key_of(&program(dir.path()), "x\n", dir.path());

        cache.put(key, &loud(STDERR_CAP));

        let stored = cache.get(key).expect("the entry should read back");
        assert_eq!(stored.stderr.len(), STDERR_CAP);
    }

    #[test]
    fn stderr_one_byte_over_the_cap_is_not_stored() {
        let dir = temp();
        let cache =
            Cache::at(dir.path(), dir.path().join("cache"), 10).expect("the cache should open");
        let key = key_of(&program(dir.path()), "x\n", dir.path());

        cache.put(key, &loud(STDERR_CAP + 1));

        assert!(
            cache.get(key).is_none(),
            "a cut stderr must never be cached"
        );
    }

    #[test]
    fn prune_removes_the_oldest_entries_beyond_the_limit() {
        let dir = temp();
        let cache =
            Cache::at(dir.path(), dir.path().join("cache"), 2).expect("the cache should open");
        let program = program(dir.path());
        let keys: Vec<Key> = ["a\n", "b\n", "c\n"]
            .into_iter()
            .map(|input| key_of(&program, input, dir.path()))
            .collect();
        for (age, key) in keys.iter().enumerate() {
            cache.put(*key, &ran());
            let file = fs::File::options()
                .write(true)
                .open(cache.entry(*key).expect("a key names one component"))
                .expect("the entry should open");
            let at = UNIX_EPOCH + std::time::Duration::from_secs(1_000 + age as u64);
            file.set_modified(at).expect("the time should be settable");
        }

        cache.prune();

        let kept: Vec<bool> = keys.iter().map(|key| cache.get(*key).is_some()).collect();
        assert_eq!(kept, [false, true, true]);
    }

    /// The names directly inside `dir`, sorted.
    fn names(dir: &Path) -> Vec<OsString> {
        let mut names: Vec<OsString> = fs::read_dir(dir)
            .expect("the directory should be listable")
            .map(|entry| entry.expect("an entry should be readable").file_name())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn put_leaves_an_entry_that_is_a_symlink_to_outside_and_its_target_alone() {
        use crate::cli::testing::symlink;
        let dir = temp();
        let cache =
            Cache::at(dir.path(), dir.path().join("cache"), 10).expect("the cache should open");
        let key = key_of(&program(dir.path()), "echo hi\n", dir.path());
        let outside = dir.path().join("outside");
        fs::write(&outside, "outside\n").expect("the outside file should be writable");
        symlink(
            &outside,
            cache.entry(key).expect("a key names one component"),
        )
        .expect("the entry symlink should be creatable");

        cache.put(key, &ran());

        assert_eq!(
            fs::read_to_string(&outside).expect("the outside file should be readable"),
            "outside\n"
        );
        assert!(
            fs::symlink_metadata(cache.entry(key).expect("a key names one component"))
                .expect("the entry should still exist")
                .is_symlink(),
            "the symlinked entry should not be replaced"
        );
        assert_eq!(names(&cache.dir), [OsString::from(key.0.to_string())]);
    }

    #[test]
    fn a_cache_directory_replaced_by_a_symlink_to_outside_is_not_written_or_pruned() {
        use crate::cli::testing::symlink;
        let dir = temp();
        let cache =
            Cache::at(dir.path(), dir.path().join("cache"), 1).expect("the cache should open");
        let key = key_of(&program(dir.path()), "echo hi\n", dir.path());
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).expect("the outside directory should be creatable");
        for name in ["a", "b", "c"] {
            fs::write(outside.join(name), name).expect("an outside file should be writable");
        }
        fs::rename(&cache.dir, dir.path().join("moved")).expect("the cache should move aside");
        symlink(&outside, &cache.dir).expect("the swapped-in symlink should be creatable");

        cache.put(key, &ran());
        cache.prune();

        assert_eq!(
            names(&outside),
            ["a", "b", "c"].map(OsString::from),
            "nothing should be written or removed where the symlink leads"
        );
    }

    /// The tool environment with `XDG_CACHE_HOME` set to `base` and nothing else.
    fn xdg(base: &Path) -> ToolEnv {
        let base = base.to_path_buf();
        ToolEnv::from_vars(|name| (name == "XDG_CACHE_HOME").then(|| base.clone().into()))
    }

    #[test]
    fn a_link_beneath_the_cache_base_that_leads_out_gives_no_cache_and_creates_nothing_there() {
        use crate::cli::testing::symlink;
        let dir = temp();
        let root = dir.path().join("root");
        let base = dir.path().join("base");
        let outside = dir.path().join("outside");
        for made in [&root, &base, &outside] {
            fs::create_dir(made).expect("a fixture directory should be creatable");
        }
        symlink(&outside, base.join("supdoc")).expect("the planted link");

        let cache = Cache::open(&xdg(&base), &root);

        assert!(cache.is_none(), "{cache:?}");
        assert_eq!(
            names(&outside),
            Vec::<OsString>::new(),
            "nothing where the link leads"
        );
    }

    #[test]
    fn a_checkout_beneath_the_cache_base_gives_no_cache_and_creates_nothing_in_it() {
        let dir = temp();
        let base = dir.path().join("base");
        let root = base.join("supdoc");
        fs::create_dir_all(&root).expect("the root should be creatable");

        let cache = Cache::open(&xdg(&base), &root);

        assert!(cache.is_none(), "{cache:?}");
        assert_eq!(
            names(&root),
            Vec::<OsString>::new(),
            "nothing in the checkout"
        );
    }

    #[test]
    fn a_cache_base_that_is_a_link_to_another_dir_gives_a_cache_under_where_it_leads() {
        use crate::cli::testing::symlink;
        let dir = temp();
        let root = dir.path().join("root");
        let disk = dir.path().join("disk");
        let base = dir.path().join("base");
        fs::create_dir(&root).expect("the root should be creatable");
        fs::create_dir(&disk).expect("the link's target should be creatable");
        symlink(&disk, &base).expect("the cache base link");

        let cache = Cache::open(&xdg(&base), &root).expect("a linked base should give a cache");

        let real = disk.canonicalize().expect("the disk should resolve");
        assert!(
            cache.dir.starts_with(real.join("supdoc/code-blocks")),
            "{cache:?}"
        );
        assert!(
            cache.dir.is_dir(),
            "the cache should be created where the base leads"
        );
    }

    /// The variable `cache_bases` falls back to on this OS: `LOCALAPPDATA` on Windows, else `HOME`.
    fn home_variable() -> &'static str {
        if cfg!(windows) {
            "LOCALAPPDATA"
        } else {
            "HOME"
        }
    }

    /// The cache base a home directory gives: the directory itself on Windows, `.cache` in it elsewhere.
    fn home_cache() -> &'static str {
        if cfg!(windows) { "" } else { ".cache" }
    }

    /// A scratch home holding `real` (a plain directory) and `link` (to `elsewhere/inner`), so
    /// `real/..` and `link/..` both hold a `..`, with those two values.
    fn hostile_home() -> (tempfile::TempDir, [PathBuf; 2]) {
        use crate::cli::testing::symlink;
        let dir = temp();
        fs::create_dir(dir.path().join("real")).expect("the plain directory should be creatable");
        fs::create_dir_all(dir.path().join("elsewhere/inner"))
            .expect("the link's destination should be creatable");
        symlink(dir.path().join("elsewhere/inner"), dir.path().join("link"))
            .expect("the link should be creatable");
        let values = [
            dir.path().join("real").join(".."),
            dir.path().join("link").join(".."),
        ];
        (dir, values)
    }

    /// Every path beneath `dir`, links not followed, to show nothing was created there.
    fn tree(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            for entry in fs::read_dir(&next).expect("a scratch directory should be listable") {
                let path = entry.expect("a scratch entry should be readable").path();
                if path.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
                    pending.push(path.clone());
                }
                found.push(path);
            }
        }
        found.sort();
        found
    }

    #[test]
    fn a_cache_base_holding_dot_dot_gives_no_cache_and_creates_nothing() {
        let (home, values) = hostile_home();
        let root = temp();
        let before = tree(home.path());

        for (variable, value) in values
            .iter()
            .flat_map(|value| [("XDG_CACHE_HOME", value), ("HOME", value)])
        {
            let env = ToolEnv::from_vars(|name| (name == variable).then(|| value.clone().into()));

            let cache = Cache::open(&env, root.path());

            assert!(
                cache.is_none(),
                "{variable}={} should give no cache: {cache:?}",
                value.display()
            );
            assert_eq!(
                tree(home.path()),
                before,
                "{variable}={} created a file",
                value.display()
            );
        }
    }

    #[test]
    fn a_cache_home_holding_dot_dot_falls_back_to_the_platforms_home_cache() {
        let (scratch, values) = hostile_home();
        let root = temp();
        let home = scratch.path().join("home");
        let env = |xdg: Option<&PathBuf>| {
            let vars = [
                ("XDG_CACHE_HOME", xdg.map(|path| path.as_os_str())),
                (home_variable(), Some(home.as_os_str())),
            ];
            ToolEnv::from_vars(|name| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .and_then(|(_, value)| value.map(OsStr::to_os_string))
            })
        };

        let real = scratch
            .path()
            .canonicalize()
            .expect("the scratch home should resolve");
        for xdg in values.iter().map(Some).chain([None]) {
            let cache = Cache::open(&env(xdg), root.path())
                .expect("the home cache should open when XDG_CACHE_HOME is unusable");
            assert!(
                cache.dir.starts_with(
                    real.join("home")
                        .join(home_cache())
                        .join("supdoc/code-blocks")
                ),
                "XDG_CACHE_HOME={xdg:?}: {cache:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn open_uses_xdg_then_home_and_creates_the_directory_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp();
        let root = dir.path().join("root");
        fs::create_dir(&root).expect("the root should be creatable");
        let xdg = dir.path().join("xdg");
        let home = dir.path().join("home");
        let env = |vars: Vec<(&'static str, OsString)>| {
            ToolEnv::from_vars(|name| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone())
            })
        };

        let both = Cache::open(
            &env(vec![
                ("XDG_CACHE_HOME", xdg.clone().into()),
                ("HOME", home.clone().into()),
            ]),
            &root,
        )
        .expect("the XDG cache should open");
        let relative = Cache::open(
            &env(vec![
                ("XDG_CACHE_HOME", "relative".into()),
                ("HOME", home.clone().into()),
            ]),
            &root,
        )
        .expect("the home cache should open");
        let neither = Cache::open(&env(vec![]), &root);
        let inside = Cache::open(&env(vec![("HOME", root.clone().into())]), &root);

        let real = dir
            .path()
            .canonicalize()
            .expect("the temp dir should resolve");
        assert!(
            both.dir.starts_with(real.join("xdg/supdoc/code-blocks")),
            "{both:?}"
        );
        assert!(
            relative
                .dir
                .starts_with(real.join("home/.cache/supdoc/code-blocks")),
            "{relative:?}"
        );
        assert!(neither.is_none() && inside.is_none());
        let mode = fs::metadata(&both.dir)
            .expect("the cache directory should exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
