//! Building the project crate for clippy: `cargo check` on the root's own `Cargo.toml`, into a
//! supdoc-owned target directory outside the root, and the `--extern` flags its artifacts name.
//!
//! The manifest is read only through an in-root real path, as the toolchain reader reads it.
//! cargo runs from the root, with the cleared environment every tool gets plus the toolchain's
//! variables, `CARGO_TARGET_DIR` and `CARGO_INCREMENTAL=0`, and nothing else.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
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
    /// No `Cargo.toml`, or a root package with no lib target: the block compiles alone.
    None,
    /// The `Cargo.toml` leaves the root, isn't a regular file, or can't be read: nothing is built.
    Refused(Refusal),
    /// A root `[package]` with a lib target: that lib, by the manifest's real path.
    Package(PathBuf),
    /// A virtual workspace: every member's lib, by the manifest's real path.
    Workspace(PathBuf),
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
/// Effect: reads the manifest's metadata, resolves symlinks, reads at most 64 KiB of it, and
/// checks whether `src/lib.rs` is a file.
#[must_use]
pub(crate) fn plan(root: &Path) -> Plan {
    let file = root.join("Cargo.toml");
    if std::fs::symlink_metadata(&file).is_err() {
        return Plan::None;
    }
    let text = match toolchain::read_inside(&file, root) {
        Ok(text) => text,
        Err(refusal) => return Plan::Refused(refusal),
    };
    // Resolved again for cargo's argv, and judged again in case it moved since the read.
    let Some(manifest) = file.canonicalize().ok().zip(root.canonicalize().ok()) else {
        return Plan::Refused(Refusal::Unreadable);
    };
    let (manifest, real_root) = manifest;
    if !manifest.starts_with(&real_root) {
        return Plan::Refused(Refusal::Outside);
    }
    let Ok(table) = text.parse::<toml::Table>() else {
        return Plan::Package(manifest);
    };
    if table.contains_key("package") {
        if table.contains_key("lib") || root.join("src/lib.rs").is_file() {
            Plan::Package(manifest)
        } else {
            Plan::None
        }
    } else if table.contains_key("workspace") {
        Plan::Workspace(manifest)
    } else {
        Plan::Package(manifest)
    }
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
/// read or name no project lib.
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
    let externs = externs(&ran.stdout, select, target).map_err(BuildFailed::ToolFailed)?;
    let fingerprint = fingerprint(&externs).map_err(BuildFailed::ToolFailed)?;
    Ok(Build {
        externs,
        deps: target.join("debug/deps"),
        fingerprint,
    })
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

/// The project's externs in cargo's artifact JSON `stdout`: one `(crate name, file)` per
/// `compiler-artifact` whose package `select` claims and whose target is a lib, rlib or proc
/// macro, naming its `.rmeta`, else its `.rlib`, else its dynamic library. A name two artifacts
/// claim with different files is left out. Sorted by name.
///
/// # Errors
///
/// A detail when a line isn't JSON, an artifact lacks a field, names no usable file or a file
/// outside `target`, or no project lib is found.
fn externs(
    stdout: &[u8],
    select: Select<'_>,
    target: &Path,
) -> Result<Vec<(String, PathBuf)>, String> {
    let mut found: BTreeMap<String, Option<PathBuf>> = BTreeMap::new();
    let lines = stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.trim_ascii().is_empty());
    for line in lines {
        let message: Value = serde_json::from_slice(line)
            .map_err(|error| format!("cargo printed a line that isn't JSON: {error}"))?;
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
        if !kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| LIB_KINDS.contains(&kind))
        {
            continue;
        }
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
        if !file.starts_with(target) {
            return Err(format!(
                "cargo named `{}` for `{name}`, outside supdoc's target directory",
                file.display()
            ));
        }
        let _claimed = found
            .entry(name)
            .and_modify(|seen| {
                if seen.as_ref() != Some(&file) {
                    *seen = None;
                }
            })
            .or_insert(Some(file));
    }
    if found.is_empty() {
        return Err("cargo built no library for the project".to_owned());
    }
    Ok(found
        .into_iter()
        .filter_map(|(name, file)| file.map(|file| (name, file)))
        .collect())
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
        BUILD_CAP, Build, BuildFailed, Builder, Plan, Select, argv, build, externs, plan,
        target_dir,
    };
    use crate::code_blocks::toolchain::{Refusal, Toolchain};
    use crate::code_blocks::tools::ToolEnv;

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

    /// The externs `lines` name, read as a package at `/p/Cargo.toml` into `/t`.
    fn package_externs(lines: &[String]) -> Result<Vec<(String, PathBuf)>, String> {
        externs(
            lines.join("\n").as_bytes(),
            Select::Package(Path::new("/p/Cargo.toml")),
            Path::new("/t"),
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
            Path::new("/t"),
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
    fn malformed_json_is_a_failure() {
        let lines = [
            artifact(
                "/p/Cargo.toml",
                "p",
                &["lib"],
                &["/t/debug/deps/libp-1.rmeta"],
            ),
            "{\"reason\":".to_owned(),
        ];

        let result = package_externs(&lines);

        assert!(
            result
                .as_ref()
                .is_err_and(|detail| detail.contains("isn't JSON")),
            "{result:?}"
        );
    }

    #[test]
    fn an_artifact_outside_the_target_dir_is_a_failure() {
        let lines = [artifact(
            "/p/Cargo.toml",
            "p",
            &["lib"],
            &["/p/target/libp-1.rmeta"],
        )];

        let result = package_externs(&lines);

        assert!(
            result
                .as_ref()
                .is_err_and(|detail| detail.contains("outside")),
            "{result:?}"
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

    /// A fresh temporary directory holding an empty `root`.
    fn root_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
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
        assert_eq!(plan(&root), Plan::Refused(Refusal::Outside));
        fs::remove_file(root.join("Cargo.toml")).expect("the symlink should be removable");
        fs::create_dir(root.join("Cargo.toml")).expect("the directory should be creatable");
        assert_eq!(
            plan(&root),
            Plan::Refused(Refusal::Unreadable),
            "not a file"
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
        let cargo = bin.join("cargo");
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > '{record}/argv'\n\
             /usr/bin/env > '{record}/env'\n\
             pwd -P > '{record}/pwd'\n\
             deps=\"$CARGO_TARGET_DIR/debug/deps\"\n\
             mkdir -p \"$deps\"\n\
             printf 'rmeta' > \"$deps/libproj-1.rmeta\"\n\
             printf '{{\"reason\":\"compiler-artifact\",\"manifest_path\":\"{manifest}\",\
             \"target\":{{\"name\":\"proj\",\"kind\":[\"lib\"]}},\
             \"filenames\":[\"%s\"]}}\\n' \"$deps/libproj-1.rmeta\"\n",
            record = record.display(),
            manifest = manifest.display(),
        );
        fs::write(&cargo, script).expect("the stub should be writable");
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
        cargo
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
            "CARGO_INCREMENTAL=0".to_owned(),
        ];
        env_expected.sort_unstable();
        let rmeta = target.path().join("debug/deps/libproj-1.rmeta");

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
        assert_eq!(deps, target.path().join("debug/deps"));
        assert_ne!(fingerprint, [0; 32]);
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
