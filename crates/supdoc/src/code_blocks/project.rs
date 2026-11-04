//! Building the project crate for clippy: `cargo check` on the root's own `Cargo.toml`, into a
//! supdoc-owned target directory outside the root, and the `--extern` flags its artifacts name.
//!
//! The manifest is read only through an in-root real path, as the toolchain reader reads it.
//! A root package that would join a workspace outside the root is refused. cargo runs from the
//! root, with the cleared environment every tool gets plus the toolchain's variables,
//! `CARGO_TARGET_DIR`, `CARGO_BUILD_BUILD_DIR` and `CARGO_INCREMENTAL=0`, and nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::format;
use super::runner::{self, Call, Cwd};
use super::toolchain::{self, Refusal};
use super::tools::ToolEnv;

/// The most bytes supdoc reads from each of cargo's stdout and stderr during the build: its
/// artifact JSON grows with the dependency count.
pub(crate) const BUILD_CAP: usize = 16 * 1024 * 1024;

/// What the root's `Cargo.toml` says to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// No `Cargo.toml`, or a root package with no lib a block can name (none, or one whose crate
    /// types include none of `lib`, `rlib` and `proc-macro`): the block compiles alone.
    None,
    /// The `Cargo.toml` can't be used, or would build the root as part of an outside workspace:
    /// nothing is built.
    Refused(Refused),
    /// A root `[package]`, or the legacy `[project]` cargo still reads as it, with a lib target:
    /// that lib, by the manifest's real path.
    Package(PathBuf),
    /// A virtual workspace: every member's lib, by the manifest's real path.
    Workspace(PathBuf),
}

/// Why the root's `Cargo.toml` builds nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The `Cargo.toml` itself leaves the root, isn't a regular file, or can't be read.
    Manifest(Refusal),
    /// The `Cargo.toml` has both `[package]` and its legacy name `[project]`.
    Ambiguous,
    /// The root package would be built as part of a workspace outside the root.
    Workspace(OutsideWorkspace),
}

/// How a root package would join a workspace outside the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OutsideWorkspace {
    /// It sets `package.workspace`.
    PackageKey,
    /// This `Cargo.toml` above the root, by absolute path, has a `workspace` key.
    Ancestor(PathBuf),
    /// This `Cargo.toml` above the root, by absolute path, isn't a regular file of at most 64 KiB
    /// that parses, so whether it has a `workspace` key is unknown.
    Unreadable(PathBuf),
}

impl OutsideWorkspace {
    /// Why nothing is built, naming the outside manifest when there is one.
    #[must_use]
    pub(crate) fn reason(&self) -> String {
        match self {
            Self::PackageKey => {
                "`Cargo.toml` sets `package.workspace`, which builds it as part of \
                                 a workspace outside the trusted directory"
                    .to_owned()
            }
            Self::Ancestor(file) => format!(
                "`Cargo.toml` joins the cargo workspace at `{}`, outside the trusted directory",
                file.display()
            ),
            Self::Unreadable(file) => format!(
                "can't tell whether `{}` makes the project part of a workspace outside the \
                 trusted directory",
                file.display()
            ),
        }
    }

    /// What the user can change so the project builds.
    #[must_use]
    pub(crate) const fn help(&self) -> &'static str {
        match self {
            Self::PackageKey => {
                "remove `package.workspace`, or trust the workspace's directory instead"
            }
            Self::Ancestor(_) | Self::Unreadable(_) => {
                "add an empty `[workspace]` table to the project's `Cargo.toml` so it builds on \
                 its own, or trust the workspace's directory instead"
            }
        }
    }
}

/// A plan that builds something: the manifest's real path and whether every workspace member's
/// lib is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Project<'plan> {
    /// The root `Cargo.toml`'s real path.
    manifest: &'plan Path,
    /// Whether the root is a virtual workspace.
    workspace: bool,
}

impl Plan {
    /// The project this plan builds, `None` when it builds nothing.
    #[must_use]
    pub(crate) fn project(&self) -> Option<Project<'_>> {
        match self {
            Self::None | Self::Refused(_) => None,
            Self::Package(manifest) => Some(Project {
                manifest,
                workspace: false,
            }),
            Self::Workspace(manifest) => Some(Project {
                manifest,
                workspace: true,
            }),
        }
    }
}

/// What `root`'s own `Cargo.toml` says to build, read only through a real path inside `root`'s
/// real path. A manifest that doesn't parse is a [`Plan::Package`], so cargo reports what is
/// wrong with it.
///
/// A root package (`[package]`, or the legacy `[project]` cargo still reads as it) with a lib and
/// no `[workspace]` table is refused when it sets `package.workspace`, or when cargo could find
/// a workspace above it: a `Cargo.toml` above the root's real path with a `workspace` key, or one
/// that can't be read to tell. A manifest with both tables is refused. A root that doesn't parse
/// is refused for a workspace above it too, so the refusal never depends on supdoc's TOML parser
/// agreeing with cargo's.
///
/// Effect: reads the manifest's metadata, resolves symlinks, reads at most 64 KiB of it, checks
/// whether `src/lib.rs` is a file, and reads each `Cargo.toml` above the root the same way.
#[must_use]
pub(crate) fn plan(root: &Path) -> Plan {
    let file = root.join("Cargo.toml");
    if std::fs::symlink_metadata(&file).is_err() {
        return Plan::None;
    }
    let refused = |refusal| Plan::Refused(Refused::Manifest(refusal));
    let text = match toolchain::read_inside(&file, root) {
        Ok(text) => text,
        Err(refusal) => return refused(refusal),
    };
    // Resolved again for cargo's argv, and judged again in case it moved since the read.
    let Some((manifest, real_root)) = file.canonicalize().ok().zip(root.canonicalize().ok()) else {
        return refused(Refusal::Unreadable);
    };
    if !manifest.starts_with(&real_root) {
        return refused(Refusal::Outside);
    }
    let outside = |outside| Plan::Refused(Refused::Workspace(outside));
    let Ok(table) = text.parse::<toml::Table>() else {
        // Fails closed should cargo's parser accept what supdoc's doesn't.
        return ancestor_workspace(&real_root).map_or(Plan::Package(manifest), outside);
    };
    // cargo still reads the legacy `[project]` as `[package]`.
    let package = match (table.get("package"), table.get("project")) {
        (Some(_), Some(_)) => return Plan::Refused(Refused::Ambiguous),
        (Some(package), None) | (None, Some(package)) => package,
        (None, None) => {
            return if table.contains_key("workspace") {
                Plan::Workspace(manifest)
            } else {
                Plan::Package(manifest)
            };
        }
    };
    if !has_lib(&table, root) {
        return Plan::None;
    }
    if table.contains_key("workspace") {
        return Plan::Package(manifest);
    }
    let joins = if package
        .as_table()
        .is_some_and(|package| package.contains_key("workspace"))
    {
        Some(OutsideWorkspace::PackageKey)
    } else {
        ancestor_workspace(&real_root)
    };
    joins.map_or(Plan::Package(manifest), outside)
}

/// Whether the root package in `table` has a lib a block can name: a `[lib]` table or a
/// `src/lib.rs` file under `root`, whose `crate-type`, when given, includes one of
/// [`LIB_KINDS`]: `lib`, `rlib` or `proc-macro`.
///
/// Effect: checks whether `src/lib.rs` is a file.
fn has_lib(table: &toml::Table, root: &Path) -> bool {
    let lib = table.get("lib");
    if lib.is_none() && !root.join("src/lib.rs").is_file() {
        return false;
    }
    let types = lib
        .and_then(toml::Value::as_table)
        .and_then(|lib| lib.get("crate-type").or_else(|| lib.get("crate_type")))
        .and_then(toml::Value::as_array);
    types.is_none_or(|types| {
        types
            .iter()
            .filter_map(toml::Value::as_str)
            .any(|kind| LIB_KINDS.contains(&kind))
    })
}

/// The most bytes read from a `Cargo.toml` above the root: the toolchain reader's cap.
const ANCESTOR_CAP: u64 = 64 * 1024;

/// The first `Cargo.toml` above `real_root`, walking to `/`, that would let cargo build the root
/// package as part of an outside workspace: one with a `workspace` key, or one that isn't a
/// regular file of at most 64 KiB that parses. A `Cargo.toml` with no `workspace` key is walked
/// past, as cargo walks past it.
///
/// Effect: reads the metadata of each `Cargo.toml` above `real_root`, and at most 64 KiB of
/// each one that is a regular file.
fn ancestor_workspace(real_root: &Path) -> Option<OutsideWorkspace> {
    real_root
        .ancestors()
        .skip(1)
        .map(|dir| dir.join("Cargo.toml"))
        .filter(|file| std::fs::symlink_metadata(file).is_ok())
        .find_map(|file| match read_table(&file) {
            Some(table) if !table.contains_key("workspace") => None,
            Some(_) => Some(OutsideWorkspace::Ancestor(file)),
            None => Some(OutsideWorkspace::Unreadable(file)),
        })
}

/// `file` as a TOML table, `None` when it isn't a regular file, can't be read as UTF-8, is over
/// [`ANCESTOR_CAP`], or doesn't parse. The metadata is checked before opening, so a FIFO can't
/// block the read.
///
/// Effect: reads the file's metadata and at most [`ANCESTOR_CAP`] bytes of it.
fn read_table(file: &Path) -> Option<toml::Table> {
    if !std::fs::metadata(file).ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    let _read = std::fs::File::open(file)
        .ok()?
        .take(ANCESTOR_CAP + 1)
        .read_to_string(&mut text)
        .ok()?;
    if !u64::try_from(text.len()).is_ok_and(|len| len <= ANCESTOR_CAP) {
        return None;
    }
    text.parse().ok()
}

/// The directory cargo builds into: `<base>/supdoc/code-blocks/target/<sha256 of the root's real
/// path>`, created mode 0700 and only ever outside the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetDir(PathBuf);

impl TargetDir {
    /// The directory's path.
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

/// The target directory for `root`, under the absolute `XDG_CACHE_HOME`, else under the absolute
/// `HOME`'s `.cache`. `None` when neither is usable: not absolute, inside `root`, or not a
/// directory only its owner reaches.
///
/// Effect: resolves `root`, creates the directory and reads its metadata.
#[must_use]
pub(crate) fn target_dir(env: &ToolEnv, root: &Path) -> Option<TargetDir> {
    let real_root = root.canonicalize().ok()?;
    let hash = hex(&Sha256::digest(real_root.as_os_str().as_encoded_bytes()));
    let var = |name: &str| {
        env.var(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let candidates = [
        var("XDG_CACHE_HOME"),
        var("HOME").map(|home| home.join(".cache")),
    ];
    candidates
        .into_iter()
        .flatten()
        .map(|cache| cache.join("supdoc/code-blocks/target").join(&hash))
        .find(|dir| private(dir, &real_root))
        .map(TargetDir)
}

/// Whether `dir`, created mode 0700 when missing, is a directory only its owner reaches, outside
/// `real_root`. Nothing is created unless the deepest existing ancestor of `dir` is outside it.
///
/// Effect: reads metadata along `dir`, then creates the directory.
fn private(dir: &Path, real_root: &Path) -> bool {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let existing = dir
        .ancestors()
        .find_map(|ancestor| ancestor.canonicalize().ok());
    if existing.is_none_or(|existing| existing.starts_with(real_root)) {
        return false;
    }
    let _created = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir);
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    let Ok(real) = dir.canonicalize() else {
        return false;
    };
    meta.is_dir() && meta.permissions().mode().trailing_zeros() >= 6 && !real.starts_with(real_root)
}

/// `bytes` as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _written = write!(text, "{byte:02x}");
        text
    })
}

/// A finished project build: what clippy-driver links each block against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Build {
    /// Each project lib as `(crate name, artifact)`, by name.
    pub(crate) externs: Vec<(String, PathBuf)>,
    /// The build's dependency directory, passed as `-L dependency=`.
    pub(crate) deps: PathBuf,
    /// The sha256 over each extern's name and bytes, which names the build in the result cache.
    pub(crate) fingerprint: [u8; 32],
}

/// Why the project didn't build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuildFailed {
    /// cargo failed, or its output couldn't be used; the detail says how.
    ToolFailed(String),
    /// cargo passed the build timeout.
    TimedOut,
}

/// The tool-side inputs of a build: cargo, the environment, the toolchain's variables, where it
/// builds, the root and the timeout.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Builder<'run> {
    /// cargo's absolute real path, as the lookup returned it.
    pub(crate) cargo: &'run Path,
    /// The environment every tool gets.
    pub(crate) env: &'run ToolEnv,
    /// The toolchain's variables: `RUSTUP_AUTO_INSTALL=0`, and `RUSTUP_TOOLCHAIN` when pinned.
    pub(crate) toolchain: &'run [(&'static str, OsString)],
    /// Where cargo builds.
    pub(crate) target: &'run TargetDir,
    /// The project root, which cargo runs from.
    pub(crate) root: &'run Path,
    /// How long the build may run. `None` is no limit.
    pub(crate) timeout: Option<Duration>,
}

/// cargo's arguments for `project`.
fn argv(project: Project<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["check".into(), "--lib".into()];
    if project.workspace {
        args.push("--workspace".into());
    }
    args.extend(
        [
            "--locked",
            "--offline",
            "--message-format=json-render-diagnostics",
            "--manifest-path",
        ]
        .map(OsString::from),
    );
    args.push(project.manifest.into());
    args
}

/// Builds `project` with `builder`'s cargo, from the root, under [`BUILD_CAP`] and the build
/// timeout, then reads the externs from its artifact JSON and fingerprints them.
///
/// Effect: spawns cargo, which runs the project's build scripts and proc macros, and reads each
/// extern's bytes.
///
/// # Errors
///
/// [`BuildFailed::TimedOut`] when cargo passes the timeout, and [`BuildFailed::ToolFailed`] when
/// it can't start, exits unsuccessfully, writes past its cap, or prints artifacts that can't be
/// read, name no project lib, or name files that aren't inside the target directory. Lines of
/// its stdout that aren't JSON are skipped, and named in that detail.
pub(crate) fn build(project: Project<'_>, builder: &Builder<'_>) -> Result<Build, BuildFailed> {
    let target = builder.target.path();
    let args = argv(project);
    let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    let mut extra: Vec<(&str, &OsStr)> = builder
        .toolchain
        .iter()
        .map(|(name, value)| (*name, value.as_os_str()))
        .collect();
    extra.push(("CARGO_TARGET_DIR", target.as_os_str()));
    // Outranks a project `build.build-dir`, which would otherwise build inside the checkout.
    extra.push(("CARGO_BUILD_BUILD_DIR", target.as_os_str()));
    extra.push(("CARGO_INCREMENTAL", OsStr::new("0")));
    let call = Call {
        program: builder.cargo,
        name: "cargo",
        args: &args,
        extra: &extra,
        stdin: b"",
        timeout: builder.timeout,
        cwd: Cwd::Root,
        cap: BUILD_CAP,
    };
    let ran = runner::run(&call, builder.env, builder.root).map_err(|failure| {
        format::failure_detail(failure).map_or(BuildFailed::TimedOut, BuildFailed::ToolFailed)
    })?;
    if !ran.success {
        return Err(BuildFailed::ToolFailed(format::exit_detail(&ran.stderr)));
    }
    let select = if project.workspace {
        Select::Members(project.manifest.parent().unwrap_or(project.manifest))
    } else {
        Select::Package(project.manifest)
    };
    let (libs, noise) = artifacts(&ran.stdout, select);
    let built = libs.and_then(|libs| {
        let (externs, deps) = locate(libs, target)?;
        let fingerprint = fingerprint(&externs)?;
        Ok(Build {
            externs,
            deps,
            fingerprint,
        })
    });
    built.map_err(|detail| BuildFailed::ToolFailed(noise.explain(detail)))
}

/// Which artifacts are the project's own.
#[derive(Debug, Clone, Copy)]
enum Select<'plan> {
    /// Those whose manifest is this real path: the root package.
    Package(&'plan Path),
    /// Those whose manifest is under this real path: the workspace's members.
    Members(&'plan Path),
}

impl Select<'_> {
    /// Whether an artifact from the package at `manifest` is the project's.
    fn claims(self, manifest: &Path) -> bool {
        match self {
            Self::Package(root_manifest) => manifest == root_manifest,
            Self::Members(root) => manifest.starts_with(root),
        }
    }
}

/// The target kinds a block can name as an extern crate.
const LIB_KINDS: [&str; 3] = ["lib", "rlib", "proc-macro"];

/// A project lib cargo built: its crate name, the file a block links against, and whether it is
/// a proc macro, which cargo builds for the host.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Lib {
    /// The crate name, with `-` as `_`.
    name: String,
    /// The `.rmeta`, else the `.rlib`, else the dynamic library, as cargo named it.
    file: PathBuf,
    /// Whether the target is a proc macro.
    proc_macro: bool,
}

/// How many of the stdout lines that aren't JSON a failure's detail names.
const NOISE_LINES: usize = 8;

/// The most characters of each such line the detail keeps.
const NOISE_WIDTH: usize = 200;

/// The lines of cargo's stdout that aren't JSON objects: what a rustc wrapper or a printing proc
/// macro wrote. The first [`NOISE_LINES`] are kept, each cut to [`NOISE_WIDTH`] characters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Noise {
    /// The first lines kept.
    kept: Vec<String>,
    /// How many lines there were in all.
    count: usize,
}

impl Noise {
    /// Notes `line`, keeping it while fewer than [`NOISE_LINES`] are kept.
    fn note(&mut self, line: &[u8]) {
        self.count += 1;
        if self.kept.len() < NOISE_LINES {
            self.kept.push(
                String::from_utf8_lossy(line)
                    .chars()
                    .take(NOISE_WIDTH)
                    .collect(),
            );
        }
    }

    /// `detail`, followed by the kept lines when there were any.
    fn explain(&self, detail: String) -> String {
        if self.count == 0 {
            return detail;
        }
        let mut text = format!(
            "{detail}\ncargo also printed {} line(s) that aren't JSON, first:",
            self.count
        );
        for line in &self.kept {
            text.push('\n');
            text.push_str(line);
        }
        text
    }
}

/// The project's libs in cargo's artifact JSON `stdout`: one per `compiler-artifact` whose
/// package `select` claims and whose target is a lib, rlib or proc macro, naming its `.rmeta`,
/// else its `.rlib`, else its dynamic library. A name two artifacts claim with different files
/// is left out. Sorted by name. A line that isn't a JSON object is skipped and noted in the
/// [`Noise`].
///
/// # Errors
///
/// A detail when an artifact lacks a field or names no usable file, or no project lib is found.
fn artifacts(stdout: &[u8], select: Select<'_>) -> (Result<Vec<Lib>, String>, Noise) {
    let mut noise = Noise::default();
    let libs = libs(stdout, select, &mut noise);
    (libs, noise)
}

/// [`artifacts`]'s libs, noting each line that isn't a JSON object in `noise`.
///
/// # Errors
///
/// As [`artifacts`].
fn libs(stdout: &[u8], select: Select<'_>, noise: &mut Noise) -> Result<Vec<Lib>, String> {
    let mut found: BTreeMap<String, Option<(PathBuf, bool)>> = BTreeMap::new();
    let lines = stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.trim_ascii().is_empty());
    for line in lines {
        let Some(message) = serde_json::from_slice::<Value>(line)
            .ok()
            .filter(Value::is_object)
        else {
            noise.note(line);
            continue;
        };
        if message.get("reason").and_then(Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        let missing = |field: &str| format!("a cargo artifact has no `{field}`");
        let manifest = message
            .get("manifest_path")
            .and_then(Value::as_str)
            .ok_or_else(|| missing("manifest_path"))?;
        if !select.claims(Path::new(manifest)) {
            continue;
        }
        let kinds = message
            .pointer("/target/kind")
            .and_then(Value::as_array)
            .ok_or_else(|| missing("target.kind"))?;
        let kinds: Vec<&str> = kinds.iter().filter_map(Value::as_str).collect();
        if !kinds.iter().any(|kind| LIB_KINDS.contains(kind)) {
            continue;
        }
        let proc_macro = kinds.contains(&"proc-macro");
        let name = message
            .pointer("/target/name")
            .and_then(Value::as_str)
            .ok_or_else(|| missing("target.name"))?
            .replace('-', "_");
        let files: Vec<&str> = message
            .get("filenames")
            .and_then(Value::as_array)
            .ok_or_else(|| missing("filenames"))?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let file =
            pick(&files).ok_or_else(|| format!("cargo built no usable file for `{name}`"))?;
        let lib = (file, proc_macro);
        let _claimed = found
            .entry(name)
            .and_modify(|seen| {
                if seen.as_ref() != Some(&lib) {
                    *seen = None;
                }
            })
            .or_insert(Some(lib));
    }
    if found.is_empty() {
        return Err("cargo built no library for the project".to_owned());
    }
    Ok(found
        .into_iter()
        .filter_map(|(name, lib)| {
            lib.map(|(file, proc_macro)| Lib {
                name,
                file,
                proc_macro,
            })
        })
        .collect())
}

/// Each lib as `(crate name, real path)`, every one a regular file inside `target`'s real path,
/// and the dependency directory: the one directory holding the libs that aren't proc macros,
/// else the one holding the proc macros. That is `<target>/<triple>/debug/deps` when the project
/// sets `build.target`, since the proc macros stay in the host's `<target>/debug/deps`.
///
/// Effect: resolves `target` and each lib's file.
///
/// # Errors
///
/// A detail when `target` doesn't resolve, a file doesn't resolve to a regular file inside it,
/// there are no libs, or the libs sit in more than one directory.
fn locate(libs: Vec<Lib>, target: &Path) -> Result<(Vec<(String, PathBuf)>, PathBuf), String> {
    let real_target = target.canonicalize().map_err(|error| {
        format!(
            "couldn't resolve supdoc's target directory `{}`: {error}",
            target.display()
        )
    })?;
    let mut externs = Vec::with_capacity(libs.len());
    let mut lib_dirs = BTreeSet::new();
    let mut macro_dirs = BTreeSet::new();
    for Lib {
        name,
        file,
        proc_macro,
    } in libs
    {
        let outside = || {
            format!(
                "cargo named `{}` for `{name}`, which isn't a file inside supdoc's target \
                 directory",
                file.display()
            )
        };
        let real = file
            .canonicalize()
            .ok()
            .filter(|real| real.starts_with(&real_target) && real.is_file())
            .ok_or_else(outside)?;
        let dir = real.parent().ok_or_else(outside)?.to_path_buf();
        let _new = if proc_macro {
            macro_dirs.insert(dir)
        } else {
            lib_dirs.insert(dir)
        };
        externs.push((name, real));
    }
    let dirs = if lib_dirs.is_empty() {
        macro_dirs
    } else {
        lib_dirs
    };
    let mut dirs = dirs.into_iter();
    match (dirs.next(), dirs.next()) {
        (Some(deps), None) => Ok((externs, deps)),
        (None, _) => Err("each library cargo built for the project shares its name".to_owned()),
        (Some(_), Some(_)) => {
            Err("cargo put the project's libraries in more than one directory".to_owned())
        }
    }
}

/// The file an extern names among `files`: the `.rmeta`, else the `.rlib`, else the dynamic
/// library a proc macro builds.
fn pick(files: &[&str]) -> Option<PathBuf> {
    ["rmeta", "rlib", "so", "dylib", "dll"]
        .into_iter()
        .find_map(|extension| {
            files
                .iter()
                .map(Path::new)
                .find(|file| file.extension().is_some_and(|found| found == extension))
        })
        .map(Path::to_path_buf)
}

/// The sha256 over each extern's name, length and bytes, in order.
///
/// Effect: reads each extern's file.
///
/// # Errors
///
/// A detail when a file can't be read.
fn fingerprint(externs: &[(String, PathBuf)]) -> Result<[u8; 32], String> {
    let mut hasher = Sha256::new();
    for (name, file) in externs {
        let bytes = std::fs::read(file)
            .map_err(|error| format!("couldn't read `{}`: {error}", file.display()))?;
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{
        BUILD_CAP, Build, BuildFailed, Builder, Lib, OutsideWorkspace, Plan, Refused, Select, argv,
        artifacts, build, locate, plan, target_dir,
    };
    use crate::code_blocks::toolchain::{Refusal, Toolchain};
    use crate::code_blocks::tools::ToolEnv;

    /// The externs `stdout` names for `select`, before they are located on disk.
    fn externs(stdout: &[u8], select: Select<'_>) -> Result<Vec<(String, PathBuf)>, String> {
        artifacts(stdout, select)
            .0
            .map(|libs| libs.into_iter().map(|lib| (lib.name, lib.file)).collect())
    }

    /// A `compiler-artifact` line for package `manifest`'s target `name` of `kinds`, naming
    /// `files`.
    fn artifact(manifest: &str, name: &str, kinds: &[&str], files: &[&str]) -> String {
        serde_json::json!({
            "reason": "compiler-artifact",
            "package_id": "x",
            "manifest_path": manifest,
            "target": { "name": name, "kind": kinds },
            "filenames": files,
            "fresh": false,
        })
        .to_string()
    }

    /// The externs `lines` name, read as a package at `/p/Cargo.toml`.
    fn package_externs(lines: &[String]) -> Result<Vec<(String, PathBuf)>, String> {
        externs(
            lines.join("\n").as_bytes(),
            Select::Package(Path::new("/p/Cargo.toml")),
        )
    }

    /// How many times [`build_retrying`] spawns a stub that Linux reports busy.
    const BUSY_ATTEMPTS: u32 = 5;

    /// [`build`], retried a few times, 20 ms apart, while a stub just written fails to start: on
    /// Linux another test's fork can still hold its write descriptor (`ETXTBSY`).
    fn build_retrying(plan: &Plan, builder: &Builder<'_>) -> Result<Build, BuildFailed> {
        let mut attempt = 1;
        loop {
            match build(plan.project().expect("the plan should build"), builder) {
                Err(BuildFailed::ToolFailed(detail))
                    if detail.starts_with("it couldn't start") && attempt < BUSY_ATTEMPTS =>
                {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                result => return result,
            }
        }
    }

    #[test]
    fn a_package_lib_names_its_rmeta_over_its_rlib() {
        let lines = [
            artifact(
                "/p/Cargo.toml",
                "my-lib",
                &["lib"],
                &[
                    "/t/debug/deps/libmy_lib-1.rlib",
                    "/t/debug/deps/libmy_lib-1.rmeta",
                ],
            ),
            r#"{"reason":"build-finished","success":true}"#.to_owned(),
        ];

        let found = package_externs(&lines).expect("the artifacts should read");

        assert_eq!(
            found,
            [(
                "my_lib".to_owned(),
                PathBuf::from("/t/debug/deps/libmy_lib-1.rmeta")
            )]
        );
    }

    #[test]
    fn a_dependency_artifact_is_no_extern() {
        let lines = [
            artifact(
                "/home/.cargo/registry/serde/Cargo.toml",
                "serde",
                &["lib"],
                &["/t/debug/deps/libserde-2.rmeta"],
            ),
            artifact(
                "/p/Cargo.toml",
                "p",
                &["lib"],
                &["/t/debug/deps/libp-1.rmeta"],
            ),
        ];

        let found = package_externs(&lines).expect("the artifacts should read");

        assert_eq!(
            found,
            [("p".to_owned(), PathBuf::from("/t/debug/deps/libp-1.rmeta"))]
        );
    }

    #[test]
    fn a_workspace_names_its_lib_and_proc_macro_members_but_not_a_bin_only_member() {
        let lines = [
            artifact(
                "/w/core/Cargo.toml",
                "core-lib",
                &["lib"],
                &["/t/debug/deps/libcore_lib-1.rmeta"],
            ),
            artifact(
                "/w/derive/Cargo.toml",
                "derive",
                &["proc-macro"],
                &["/t/debug/deps/libderive-2.dylib"],
            ),
            artifact(
                "/w/tool/Cargo.toml",
                "tool",
                &["bin"],
                &["/t/debug/deps/tool-3"],
            ),
        ];

        let found = externs(
            lines.join("\n").as_bytes(),
            Select::Members(Path::new("/w")),
        )
        .expect("the artifacts should read");

        assert_eq!(
            found,
            [
                (
                    "core_lib".to_owned(),
                    PathBuf::from("/t/debug/deps/libcore_lib-1.rmeta")
                ),
                (
                    "derive".to_owned(),
                    PathBuf::from("/t/debug/deps/libderive-2.dylib")
                ),
            ]
        );
    }

    #[test]
    fn a_name_two_artifacts_claim_is_left_out() {
        let lines = [
            artifact(
                "/p/Cargo.toml",
                "dup",
                &["lib"],
                &["/t/debug/deps/libdup-1.rmeta"],
            ),
            artifact(
                "/p/Cargo.toml",
                "dup",
                &["rlib"],
                &["/t/debug/deps/libdup-2.rmeta"],
            ),
            artifact(
                "/p/Cargo.toml",
                "kept",
                &["lib"],
                &["/t/debug/deps/libkept-3.rmeta"],
            ),
        ];

        let found = package_externs(&lines).expect("the artifacts should read");

        assert_eq!(
            found,
            [(
                "kept".to_owned(),
                PathBuf::from("/t/debug/deps/libkept-3.rmeta")
            )]
        );
    }

    #[test]
    fn a_line_that_isnt_json_between_artifacts_is_skipped() {
        let lines = [
            "not json from a rustc wrapper".to_owned(),
            artifact(
                "/p/Cargo.toml",
                "p",
                &["lib"],
                &["/t/debug/deps/libp-1.rmeta"],
            ),
            "42".to_owned(),
            "{\"reason\":".to_owned(),
        ];

        let (found, noise) = artifacts(
            lines.join("\n").as_bytes(),
            Select::Package(Path::new("/p/Cargo.toml")),
        );

        assert_eq!(
            found,
            Ok(vec![Lib {
                name: "p".to_owned(),
                file: PathBuf::from("/t/debug/deps/libp-1.rmeta"),
                proc_macro: false,
            }])
        );
        assert_eq!(
            noise.explain("detail".to_owned()),
            "detail\ncargo also printed 3 line(s) that aren't JSON, first:\n\
             not json from a rustc wrapper\n42\n{\"reason\":"
        );
    }

    #[test]
    fn a_malformed_artifact_is_still_a_failure() {
        let lines =
            [r#"{"reason":"compiler-artifact","target":{"name":"p","kind":["lib"]}}"#.to_owned()];

        let result = package_externs(&lines);

        assert_eq!(
            result,
            Err("a cargo artifact has no `manifest_path`".to_owned())
        );
    }

    /// A temporary directory holding `target/debug/deps/libp-1.rmeta`, and `outside.rmeta` beside
    /// the target directory, both regular files.
    fn target_with_files() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let target = dir.path().join("target");
        fs::create_dir_all(target.join("debug/deps")).expect("the deps dir should be creatable");
        fs::write(target.join("debug/deps/libp-1.rmeta"), "rmeta")
            .expect("the rmeta should be writable");
        fs::write(dir.path().join("outside.rmeta"), "rmeta")
            .expect("the outside file should be writable");
        (dir, target)
    }

    /// A lib named `p` at `file`.
    fn lib_at(file: PathBuf, proc_macro: bool) -> Lib {
        Lib {
            name: "p".to_owned(),
            file,
            proc_macro,
        }
    }

    #[test]
    fn an_artifact_that_resolves_outside_the_target_dir_is_a_failure() {
        let (dir, target) = target_with_files();
        std::os::unix::fs::symlink(
            dir.path().join("outside.rmeta"),
            target.join("debug/deps/liblink.rmeta"),
        )
        .expect("the symlink should be creatable");
        let files = [
            dir.path().join("outside.rmeta"),
            target.join("../outside.rmeta"),
            target.join("debug/deps/liblink.rmeta"),
            target.join("debug/deps/libmissing.rmeta"),
            target.join("debug/deps"),
        ];

        for file in files {
            let result = locate(vec![lib_at(file.clone(), false)], &target);

            assert!(
                result.as_ref().is_err_and(
                    |detail| detail.contains("isn't a file inside supdoc's target directory")
                ),
                "{}: {result:?}",
                file.display()
            );
        }
    }

    #[test]
    fn the_deps_dir_is_where_the_libs_are_not_the_host_proc_macros() {
        let (_dir, target) = target_with_files();
        let triple = target.join("aarch64-unknown-none/debug/deps");
        fs::create_dir_all(&triple).expect("the triple's deps dir should be creatable");
        fs::write(triple.join("libq-2.rmeta"), "rmeta").expect("the rmeta should be writable");
        let real = |path: PathBuf| path.canonicalize().expect("the path should resolve");
        let libs = vec![
            lib_at(target.join("debug/deps/libp-1.rmeta"), true),
            Lib {
                name: "q".to_owned(),
                file: triple.join("libq-2.rmeta"),
                proc_macro: false,
            },
        ];

        let (externs, deps) = locate(libs, &target).expect("the libs should locate");

        assert_eq!(deps, real(triple.clone()));
        assert_eq!(
            externs,
            [
                ("p".to_owned(), real(target.join("debug/deps/libp-1.rmeta"))),
                ("q".to_owned(), real(triple.join("libq-2.rmeta"))),
            ]
        );
    }

    #[test]
    fn libs_in_two_directories_are_a_failure() {
        let (_dir, target) = target_with_files();
        let other = target.join("other");
        fs::create_dir_all(&other).expect("the other dir should be creatable");
        fs::write(other.join("libq-2.rmeta"), "rmeta").expect("the rmeta should be writable");
        let libs = vec![
            lib_at(target.join("debug/deps/libp-1.rmeta"), false),
            Lib {
                name: "q".to_owned(),
                file: other.join("libq-2.rmeta"),
                proc_macro: false,
            },
        ];

        let result = locate(libs, &target);

        assert_eq!(
            result,
            Err("cargo put the project's libraries in more than one directory".to_owned())
        );
    }

    #[test]
    fn no_project_lib_is_a_failure() {
        let lines = [artifact(
            "/p/Cargo.toml",
            "p",
            &["bin"],
            &["/t/debug/deps/p-1"],
        )];

        let result = package_externs(&lines);

        assert!(
            result
                .as_ref()
                .is_err_and(|detail| detail.contains("no library")),
            "{result:?}"
        );
    }

    /// A fresh temporary directory holding an empty `root` with a `src` directory, in the system
    /// temporary directory, which must have no `Cargo.toml` above it: [`plan`] refuses a package
    /// under a workspace, as supdoc's own `Cargo.toml` is.
    fn root_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let real = dir
            .path()
            .canonicalize()
            .expect("the temporary directory should resolve");
        let above: Vec<PathBuf> = real
            .ancestors()
            .map(|ancestor| ancestor.join("Cargo.toml"))
            .filter(|file| fs::symlink_metadata(file).is_ok())
            .collect();
        assert!(
            above.is_empty(),
            "the temporary directory should have no Cargo.toml above it; set TMPDIR outside any \
             cargo project: {above:?}"
        );
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("src")).expect("the root should be creatable");
        (dir, root)
    }

    #[test]
    fn plan_reads_the_roots_own_manifest() {
        let (dir, root) = root_dir();
        let manifest = || {
            root.join("Cargo.toml")
                .canonicalize()
                .expect("the manifest should resolve")
        };

        assert_eq!(plan(&root), Plan::None, "no Cargo.toml");
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"p\"\n")
            .expect("the manifest should be writable");
        assert_eq!(plan(&root), Plan::None, "a package with no lib");
        fs::write(root.join("src/lib.rs"), "").expect("the lib should be writable");
        assert_eq!(plan(&root), Plan::Package(manifest()));
        fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"a\"]\n")
            .expect("the manifest should be writable");
        assert_eq!(plan(&root), Plan::Workspace(manifest()));
        fs::remove_file(root.join("Cargo.toml")).expect("the manifest should be removable");
        let outside = dir.path().join("Cargo.toml");
        fs::write(&outside, "[package]\nname = \"p\"\n")
            .expect("the outside file should be writable");
        std::os::unix::fs::symlink(&outside, root.join("Cargo.toml"))
            .expect("the symlink should be creatable");
        assert_eq!(
            plan(&root),
            Plan::Refused(Refused::Manifest(Refusal::Outside))
        );
        fs::remove_file(root.join("Cargo.toml")).expect("the symlink should be removable");
        fs::create_dir(root.join("Cargo.toml")).expect("the directory should be creatable");
        assert_eq!(
            plan(&root),
            Plan::Refused(Refused::Manifest(Refusal::Unreadable)),
            "not a file"
        );
    }

    #[test]
    fn a_lib_with_no_lib_or_rlib_crate_type_compiles_blocks_alone() {
        let (_dir, root) = root_dir();
        fs::write(root.join("src/lib.rs"), "").expect("the lib should be writable");
        let with_types = |types: &str| {
            fs::write(
                root.join("Cargo.toml"),
                format!("[package]\nname = \"p\"\n[lib]\ncrate-type = {types}\n"),
            )
            .expect("the manifest should be writable");
            plan(&root)
        };

        assert_eq!(with_types("[\"cdylib\"]"), Plan::None, "cdylib only");
        assert_eq!(with_types("[\"staticlib\"]"), Plan::None, "staticlib only");
        assert!(
            matches!(with_types("[\"cdylib\", \"rlib\"]"), Plan::Package(_)),
            "an rlib beside a cdylib"
        );
        assert!(
            matches!(with_types("[\"lib\"]"), Plan::Package(_)),
            "a plain lib"
        );
        assert!(
            matches!(with_types("[\"proc-macro\"]"), Plan::Package(_)),
            "a proc macro named by its crate type"
        );
    }

    /// Writes `text` to `dir/Cargo.toml`, creating `dir`.
    fn manifest_at(dir: &Path, text: &str) {
        fs::create_dir_all(dir).expect("the manifest's directory should be creatable");
        fs::write(dir.join("Cargo.toml"), text).expect("the manifest should be writable");
    }

    /// A lib package at `<dir>/ws/inner/root` under the system temporary directory, which the
    /// caller gives ancestors.
    fn nested_package() -> (tempfile::TempDir, PathBuf) {
        let (dir, _root) = root_dir();
        let root = dir.path().join("ws/inner/root");
        manifest_at(&root, "[package]\nname = \"p\"\n");
        fs::create_dir_all(root.join("src")).expect("the src dir should be creatable");
        fs::write(root.join("src/lib.rs"), "").expect("the lib should be writable");
        (dir, root)
    }

    /// `dir`'s `Cargo.toml` by its real path.
    fn real_manifest(dir: &Path) -> PathBuf {
        dir.canonicalize()
            .expect("the directory should resolve")
            .join("Cargo.toml")
    }

    #[test]
    fn an_ancestor_workspace_is_refused_by_its_path() {
        let (dir, root) = nested_package();
        let ws = dir.path().join("ws");
        manifest_at(&ws, "[workspace]\nmembers = [\"inner/root\"]\n");

        assert_eq!(
            plan(&root),
            Plan::Refused(Refused::Workspace(OutsideWorkspace::Ancestor(
                real_manifest(&ws)
            )))
        );
    }

    #[test]
    fn an_ancestor_package_with_no_workspace_above_it_builds() {
        let (dir, root) = nested_package();
        manifest_at(&dir.path().join("ws"), "[package]\nname = \"outer\"\n");

        assert_eq!(plan(&root), Plan::Package(real_manifest(&root)));
    }

    #[test]
    fn the_walk_continues_past_an_ancestor_with_no_workspace() {
        let (dir, root) = nested_package();
        manifest_at(
            &dir.path().join("ws/inner"),
            "[package]\nname = \"outer\"\n",
        );
        let ws = dir.path().join("ws");
        manifest_at(&ws, "[workspace]\n");

        assert_eq!(
            plan(&root),
            Plan::Refused(Refused::Workspace(OutsideWorkspace::Ancestor(
                real_manifest(&ws)
            )))
        );
    }

    #[test]
    fn an_empty_workspace_table_in_the_root_builds_it_on_its_own() {
        let (dir, root) = nested_package();
        manifest_at(&dir.path().join("ws"), "[workspace]\n");
        manifest_at(&root, "[package]\nname = \"p\"\n\n[workspace]\n");

        assert_eq!(plan(&root), Plan::Package(real_manifest(&root)));
    }

    #[test]
    fn package_workspace_is_refused() {
        let (_dir, root) = nested_package();
        manifest_at(&root, "[package]\nname = \"p\"\nworkspace = \"../..\"\n");

        assert_eq!(
            plan(&root),
            Plan::Refused(Refused::Workspace(OutsideWorkspace::PackageKey))
        );
    }

    #[test]
    fn a_virtual_workspace_under_an_ancestor_workspace_builds_without_a_walk() {
        let (dir, root) = nested_package();
        manifest_at(&dir.path().join("ws"), "[workspace]\n");
        manifest_at(&root, "[workspace]\nmembers = []\n");

        assert_eq!(plan(&root), Plan::Workspace(real_manifest(&root)));
    }

    #[test]
    fn an_ancestor_fifo_is_refused_without_blocking() {
        let (dir, root) = nested_package();
        let ws = dir.path().join("ws");
        let made = std::process::Command::new("mkfifo")
            .arg(ws.join("Cargo.toml"))
            .status()
            .expect("mkfifo should run");
        assert!(made.success(), "mkfifo should make the FIFO");
        let (send, receive) = std::sync::mpsc::channel();
        // A plan that opens the FIFO blocks forever; the thread is left behind, not joined.
        let _planning = std::thread::spawn(move || send.send(plan(&root)));

        let planned = receive
            .recv_timeout(Duration::from_secs(5))
            .expect("plan should return within 5 s rather than block opening the FIFO");

        assert_eq!(
            planned,
            Plan::Refused(Refused::Workspace(OutsideWorkspace::Unreadable(
                real_manifest(&ws)
            )))
        );
    }

    #[test]
    fn an_oversized_or_unparsable_ancestor_is_refused_by_its_path() {
        let (dir, root) = nested_package();
        let ws = dir.path().join("ws");
        let comment = format!("# {}\n", "x".repeat(70_000));
        let refused = || {
            Plan::Refused(Refused::Workspace(OutsideWorkspace::Unreadable(
                real_manifest(&ws),
            )))
        };

        manifest_at(&ws, &format!("[package]\nname = \"outer\"\n{comment}"));
        assert_eq!(plan(&root), refused(), "over 64 KiB, with no workspace key");
        manifest_at(&ws, "[package\n");
        assert_eq!(plan(&root), refused(), "doesn't parse");
    }

    #[test]
    fn a_legacy_project_table_is_read_as_a_package() {
        let (dir, root) = nested_package();
        let ws = dir.path().join("ws");
        manifest_at(&ws, "[workspace]\n");

        manifest_at(&root, "[project]\nname = \"p\"\n");
        assert_eq!(
            plan(&root),
            Plan::Refused(Refused::Workspace(OutsideWorkspace::Ancestor(
                real_manifest(&ws)
            ))),
            "under an ancestor workspace"
        );
        manifest_at(&root, "[project]\nname = \"p\"\nworkspace = \"..\"\n");
        assert_eq!(
            plan(&root),
            Plan::Refused(Refused::Workspace(OutsideWorkspace::PackageKey)),
            "with its own workspace key"
        );
        manifest_at(&root, "[project]\nname = \"p\"\n\n[workspace]\n");
        assert_eq!(
            plan(&root),
            Plan::Package(real_manifest(&root)),
            "with its own workspace table"
        );
        manifest_at(
            &root,
            "[project]\nname = \"p\"\n[lib]\ncrate-type = [\"cdylib\"]\n",
        );
        assert_eq!(plan(&root), Plan::None, "with a cdylib-only lib");
        manifest_at(
            &root,
            "[project]\nname = \"p\"\n\n[package]\nname = \"p\"\n",
        );
        assert_eq!(
            plan(&root),
            Plan::Refused(Refused::Ambiguous),
            "beside a package table"
        );
    }

    #[test]
    fn a_root_that_doesnt_parse_is_still_refused_under_an_ancestor_workspace() {
        let (dir, root) = nested_package();
        manifest_at(&root, "[package\n");
        assert_eq!(
            plan(&root),
            Plan::Package(real_manifest(&root)),
            "with no workspace above, cargo reports the error"
        );
        let ws = dir.path().join("ws");
        manifest_at(&ws, "[workspace]\n");

        assert_eq!(
            plan(&root),
            Plan::Refused(Refused::Workspace(OutsideWorkspace::Ancestor(
                real_manifest(&ws)
            )))
        );
    }

    #[test]
    fn each_outside_workspace_refusal_names_its_manifest_and_a_fix() {
        let file = PathBuf::from("/a/Cargo.toml");
        let ancestor = OutsideWorkspace::Ancestor(file.clone());
        let unreadable = OutsideWorkspace::Unreadable(file);

        assert_eq!(
            ancestor.reason(),
            "`Cargo.toml` joins the cargo workspace at `/a/Cargo.toml`, outside the trusted \
             directory"
        );
        assert_eq!(
            unreadable.reason(),
            "can't tell whether `/a/Cargo.toml` makes the project part of a workspace outside \
             the trusted directory"
        );
        assert_eq!(ancestor.help(), unreadable.help());
        assert!(
            ancestor
                .help()
                .starts_with("add an empty `[workspace]` table")
        );
        assert!(
            OutsideWorkspace::PackageKey
                .help()
                .starts_with("remove `package.workspace`")
        );
    }

    /// An environment of `vars`, read as `main` reads supdoc's own.
    fn env_of(vars: &[(&str, OsString)]) -> ToolEnv {
        ToolEnv::from_vars(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        })
    }

    #[test]
    fn the_target_dir_is_private_and_named_by_the_root() {
        let (dir, root) = root_dir();
        let cache = dir.path().join("cache");
        let env = env_of(&[("XDG_CACHE_HOME", cache.clone().into_os_string())]);

        let target = target_dir(&env, &root).expect("the target dir should be usable");
        let mode = fs::metadata(target.path())
            .expect("the target dir should exist")
            .permissions()
            .mode();

        assert!(
            target
                .path()
                .starts_with(cache.join("supdoc/code-blocks/target"))
        );
        assert_eq!(target.path().file_name().map(OsStr::len), Some(64));
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn a_target_dir_inside_the_root_is_refused_and_creates_nothing() {
        let (_dir, root) = root_dir();
        let env = env_of(&[
            ("XDG_CACHE_HOME", root.join("cache").into_os_string()),
            ("HOME", root.join("home").into_os_string()),
        ]);

        assert_eq!(target_dir(&env, &root), None);
        assert!(!root.join("cache").exists() && !root.join("home").exists());
    }

    /// Writes `bin/cargo`, a stub that records its argv, environment and real cwd under `record`,
    /// writes an rmeta into `CARGO_TARGET_DIR` and prints the artifact line naming it for the
    /// package at `manifest`.
    fn recording_cargo(bin: &Path, record: &Path, manifest: &Path) -> PathBuf {
        for path in [record, bin] {
            fs::create_dir_all(path).expect("the fixture directory should be creatable");
        }
        let body = format!(
            "printf '%s\\n' \"$@\" > '{record}/argv'\n\
             /usr/bin/env > '{record}/env'\n\
             pwd -P > '{record}/pwd'\n\
             {emit}",
            record = record.display(),
            emit = emit_artifact(manifest),
        );
        stub(&bin.join("cargo"), &body)
    }

    /// Shell that writes an rmeta into `CARGO_TARGET_DIR` and prints the artifact line naming it
    /// for the package at `manifest`.
    fn emit_artifact(manifest: &Path) -> String {
        format!(
            "deps=\"$CARGO_TARGET_DIR/debug/deps\"\n\
             mkdir -p \"$deps\"\n\
             printf 'rmeta' > \"$deps/libproj-1.rmeta\"\n\
             printf '{{\"reason\":\"compiler-artifact\",\"manifest_path\":\"{manifest}\",\
             \"target\":{{\"name\":\"proj\",\"kind\":[\"lib\"]}},\
             \"filenames\":[\"%s\"]}}\\n' \"$deps/libproj-1.rmeta\"\n",
            manifest = manifest.display(),
        )
    }

    /// Writes an executable `sh` script at `path` running `body`.
    fn stub(path: &Path, body: &str) -> PathBuf {
        fs::write(path, format!("#!/bin/sh\n{body}")).expect("the stub should be writable");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
        path.to_path_buf()
    }

    #[test]
    fn the_build_runs_exactly_the_planned_cargo_from_the_root_with_only_its_variables() {
        let (dir, root) = root_dir();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"proj\"\n")
            .expect("the manifest should be writable");
        fs::write(root.join("src/lib.rs"), "").expect("the lib should be writable");
        let plan = plan(&root);
        let manifest = root
            .join("Cargo.toml")
            .canonicalize()
            .expect("the manifest should resolve");
        let real_root = root.canonicalize().expect("the root should resolve");
        let record = dir.path().join("record");
        let bin = dir.path().join("bin");
        let cargo = recording_cargo(&bin, &record, &manifest);
        let cache = dir.path().join("cache");
        let path = OsString::from(format!("{}:/bin:/usr/bin", bin.display()));
        // Stands in for supdoc's own environment, as `main` reads it: the build variables and a
        // secret in it must not reach cargo.
        let process: Vec<(&str, OsString)> = vec![
            ("PATH", path.clone()),
            ("HOME", dir.path().join("home").into_os_string()),
            ("XDG_CACHE_HOME", cache.into_os_string()),
            ("RUSTFLAGS", "-Cpanic=abort".into()),
            ("CARGO_ENCODED_RUSTFLAGS", "-Cdebuginfo=2".into()),
            ("SUPDOC_TEST_SECRET", "hunter2".into()),
        ];
        let env = env_of(&process);
        let target = target_dir(&env, &root).expect("the target dir should be usable");
        let toolchain = Toolchain::Channel("stable".to_owned()).env();
        let builder = Builder {
            cargo: &cargo,
            env: &env,
            toolchain: &toolchain,
            target: &target,
            root: &root,
            timeout: Some(Duration::from_secs(10)),
        };

        let built = build_retrying(&plan, &builder);
        let read = |name: &str| {
            fs::read_to_string(record.join(name)).expect("the stub should record what it saw")
        };
        let mut env_seen: Vec<String> = read("env")
            .lines()
            .filter(|line| {
                !["PWD=", "SHLVL=", "_=", "OLDPWD="]
                    .iter()
                    .any(|shell| line.starts_with(shell))
            })
            .map(str::to_owned)
            .collect();
        env_seen.sort_unstable();
        let mut env_expected = vec![
            format!("PATH={}", path.to_string_lossy()),
            format!("HOME={}", dir.path().join("home").display()),
            format!("XDG_CACHE_HOME={}", dir.path().join("cache").display()),
            "RUSTUP_AUTO_INSTALL=0".to_owned(),
            "RUSTUP_TOOLCHAIN=stable".to_owned(),
            format!("CARGO_TARGET_DIR={}", target.path().display()),
            format!("CARGO_BUILD_BUILD_DIR={}", target.path().display()),
            "CARGO_INCREMENTAL=0".to_owned(),
        ];
        env_expected.sort_unstable();
        let real_deps = target
            .path()
            .join("debug/deps")
            .canonicalize()
            .expect("the stub's deps dir should resolve");
        let rmeta = real_deps.join("libproj-1.rmeta");

        let argv_expected: Vec<String> = argv(plan.project().expect("the plan should build"))
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv_expected,
            [
                "check",
                "--lib",
                "--locked",
                "--offline",
                "--message-format=json-render-diagnostics",
                "--manifest-path",
                &manifest.display().to_string(),
            ]
        );
        assert_eq!(read("argv").lines().collect::<Vec<_>>(), argv_expected);
        assert_eq!(read("pwd").trim(), real_root.display().to_string());
        assert_eq!(env_seen, env_expected);
        let Build {
            externs,
            deps,
            fingerprint,
        } = built.expect("the stub build should succeed");
        assert_eq!(externs, [("proj".to_owned(), rmeta)]);
        assert_eq!(deps, real_deps);
        assert_ne!(fingerprint, [0; 32]);
    }

    /// Runs [`build`] for a package at `root/Cargo.toml` with a stub cargo running `body` (in
    /// which `{emit}` prints one artifact for that package) under `timeout`.
    fn stub_build(body: &str, timeout: Duration) -> Result<Build, BuildFailed> {
        let (dir, root) = root_dir();
        let manifest = root.join("Cargo.toml");
        let cargo = stub(
            &dir.path().join("cargo"),
            &body.replace("{emit}", &emit_artifact(&manifest)),
        );
        let env = env_of(&[
            ("PATH", "/bin:/usr/bin".into()),
            ("XDG_CACHE_HOME", dir.path().join("cache").into_os_string()),
        ]);
        let target = target_dir(&env, &root).expect("the target dir should be usable");
        let builder = Builder {
            cargo: &cargo,
            env: &env,
            toolchain: &[],
            target: &target,
            root: &root,
            timeout: Some(timeout),
        };
        build_retrying(&Plan::Package(manifest), &builder)
    }

    #[test]
    fn a_build_may_print_more_than_the_default_cap_under_its_own() {
        let result = stub_build(
            "head -c 2097152 /dev/zero | tr '\\0' x\necho\n{emit}",
            Duration::from_secs(10),
        );

        assert!(
            result.as_ref().is_ok_and(|built| built.externs.len() == 1),
            "{result:?}"
        );
    }

    #[test]
    fn a_build_past_its_cap_is_a_failure() {
        let result = stub_build(
            "head -c 16777217 /dev/zero\n{emit}",
            Duration::from_secs(10),
        );

        assert_eq!(
            result,
            Err(BuildFailed::ToolFailed(
                "it wrote more than 16 MiB to stdout".to_owned()
            ))
        );
    }

    #[test]
    fn a_build_past_its_timeout_timed_out() {
        let result = stub_build("exec sleep 10\n", Duration::from_millis(100));

        assert_eq!(result, Err(BuildFailed::TimedOut));
    }

    #[test]
    fn a_target_dir_others_can_reach_is_refused() {
        let (dir, root) = root_dir();
        let env = env_of(&[("XDG_CACHE_HOME", dir.path().join("cache").into_os_string())]);
        let target = target_dir(&env, &root).expect("the target dir should be usable");
        fs::set_permissions(target.path(), fs::Permissions::from_mode(0o755))
            .expect("the target dir's mode should be settable");

        assert_eq!(target_dir(&env, &root), None);
    }

    #[test]
    fn a_workspace_build_checks_every_member() {
        let manifest = PathBuf::from("/w/Cargo.toml");
        let args = argv(
            Plan::Workspace(manifest)
                .project()
                .expect("a workspace should build"),
        );

        assert_eq!(
            args.get(..3),
            Some(&["check".into(), "--lib".into(), "--workspace".into()][..])
        );
    }

    #[test]
    fn a_failing_build_carries_cargos_stderr() {
        let (dir, root) = root_dir();
        let cargo = dir.path().join("cargo");
        fs::write(
            &cargo,
            "#!/bin/sh\necho 'error: could not compile' >&2\nexit 101\n",
        )
        .expect("the stub should be writable");
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
        let env = env_of(&[
            ("PATH", "/bin:/usr/bin".into()),
            ("XDG_CACHE_HOME", dir.path().join("cache").into_os_string()),
        ]);
        let target = target_dir(&env, &root).expect("the target dir should be usable");
        let plan = Plan::Package(root.join("Cargo.toml"));
        let builder = Builder {
            cargo: &cargo,
            env: &env,
            toolchain: &[],
            target: &target,
            root: &root,
            timeout: Some(Duration::from_secs(10)),
        };

        let result = build_retrying(&plan, &builder);

        assert_eq!(
            result,
            Err(BuildFailed::ToolFailed(
                "error: could not compile".to_owned()
            ))
        );
        assert_eq!(BUILD_CAP, 16 * 1024 * 1024);
    }
}
