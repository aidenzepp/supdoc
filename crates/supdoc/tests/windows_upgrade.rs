//! `supdoc upgrade --yes` replacing the running `supdoc.exe` on Windows.
//!
//! The e2e suite is Unix only, so the Windows replace path has its own root. Each test copies the
//! binary under test into a scratch directory, runs the copy against a `SUPDOC_TEST_RELEASE_DIR`
//! fixture so the running copy replaces itself, and reads the directory afterwards.

#![cfg(windows)]
#![cfg_attr(
    test,
    allow(
        clippy::missing_errors_doc,
        clippy::missing_panics_doc,
        reason = "tests report failures rather than document callable contracts"
    )
)]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "tests build fixture and hostile paths with raw joins on purpose"
    )
)]

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output};

use sha2::{Digest as _, Sha256};

/// The version the fixture release publishes.
const NEWER: &str = "99.0.0";

/// The release platform of the binary under test.
const TRIPLE: &str = if cfg!(target_arch = "aarch64") {
    "aarch64-pc-windows-msvc"
} else {
    "x86_64-pc-windows-msvc"
};

/// The binary under test with `tag` appended, which Windows still runs, so each payload is a
/// distinct working `supdoc.exe`.
fn payload(tag: &str) -> Vec<u8> {
    let mut bytes = fs::read(env!("CARGO_BIN_EXE_supdoc")).expect("the binary should read");
    bytes.extend_from_slice(tag.as_bytes());
    bytes
}

/// A `.tar.gz` holding `bytes` as `supdoc-NEWER-TRIPLE/supdoc.exe`.
fn archive_of(bytes: &[u8]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(u64::try_from(bytes.len()).expect("the size should fit"));
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(
        &mut header,
        format!("supdoc-{NEWER}-{TRIPLE}/supdoc.exe"),
        bytes,
    )
    .expect("the entry should append");
    let tarball = tar.into_inner().expect("the tar should finish");
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&tarball).expect("the tar should compress");
    gzip.finish().expect("the gzip should finish")
}

/// Publishes `bytes` as the release archive of [`NEWER`] under `releases`.
fn publish(releases: &Path, bytes: &[u8]) {
    let archive = archive_of(bytes);
    let digest = Sha256::digest(&archive)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    let folder = releases.join(NEWER);
    fs::create_dir_all(&folder).expect("the release folder should create");
    fs::write(
        folder.join("SHA256SUMS"),
        format!("{digest}  supdoc-{NEWER}-{TRIPLE}.tar.gz\n"),
    )
    .expect("SHA256SUMS should write");
    fs::write(
        folder.join(format!("supdoc-{NEWER}-{TRIPLE}.tar.gz")),
        archive,
    )
    .expect("the archive should write");
}

/// A scratch directory holding `bin/supdoc.exe`, a copy of the binary under test, and `releases`.
fn installed() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("a scratch directory should be creatable");
    fs::create_dir(root.path().join("bin")).expect("the bin directory should create");
    let copied = fs::copy(
        env!("CARGO_BIN_EXE_supdoc"),
        root.path().join("bin").join("supdoc.exe"),
    )
    .expect("the binary should copy");
    assert_ne!(copied, 0, "the copied binary should not be empty");
    root
}

/// Runs `args` as the copy in `root`'s `bin`, with the fixture release directory set.
fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(root.join("bin").join("supdoc.exe"))
        .args(args)
        .env("SUPDOC_TEST_RELEASE_DIR", root.join("releases"))
        .output()
        .expect("the copy should start")
}

/// The names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("the directory should list")
        .map(|entry| {
            entry
                .expect("the entry should read")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn the_running_exe_replaces_itself_and_the_next_upgrade_removes_the_aside_file() {
    let root = installed();
    let bin = root.path().join("bin");
    let first = payload("first");
    publish(&root.path().join("releases"), &first);

    let ran = run(root.path(), &["upgrade", NEWER, "--yes"]);

    assert!(
        ran.status.success(),
        "the first upgrade should succeed; stderr: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(
        fs::read(bin.join("supdoc.exe")).expect("the new binary should read"),
        first
    );
    assert_eq!(names_in(&bin), ["supdoc.exe", "supdoc.exe.old"]);
    let version = Command::new(bin.join("supdoc.exe"))
        .arg("--version")
        .output()
        .expect("the new binary should run");
    assert!(
        version.status.success(),
        "the new binary should run; stderr: {}",
        String::from_utf8_lossy(&version.stderr)
    );

    let second = payload("second");
    publish(&root.path().join("releases"), &second);
    let ran = run(root.path(), &["upgrade", NEWER, "--yes"]);

    assert!(
        ran.status.success(),
        "the second upgrade should succeed; stderr: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(
        fs::read(bin.join("supdoc.exe")).expect("the newest binary should read"),
        second
    );
    assert_eq!(
        fs::read(bin.join("supdoc.exe.old")).expect("the aside file should read"),
        first,
        "the aside file is the binary the second run replaced, not the first run's original"
    );
    assert_eq!(names_in(&bin), ["supdoc.exe", "supdoc.exe.old"]);
}

#[test]
fn a_move_aside_that_cannot_happen_leaves_the_original_running_and_names_the_remedy() {
    let root = installed();
    let bin = root.path().join("bin");
    let original = fs::read(bin.join("supdoc.exe")).expect("the original should read");
    fs::create_dir(bin.join("supdoc.exe.old")).expect("the blocker should create");
    fs::write(bin.join("supdoc.exe.old").join("keep"), "x").expect("the blocker should fill");
    publish(&root.path().join("releases"), &payload("never"));

    let ran = run(root.path(), &["upgrade", NEWER, "--yes"]);

    assert!(!ran.status.success(), "the upgrade should fail");
    let stderr = String::from_utf8_lossy(&ran.stderr);
    assert!(stderr.contains("supdoc.exe.old"), "{stderr}");
    assert!(!stderr.contains("chmod"), "{stderr}");
    assert_eq!(
        fs::read(bin.join("supdoc.exe")).expect("the original should still read"),
        original
    );
    assert_eq!(names_in(&bin), ["supdoc.exe", "supdoc.exe.old"]);
}
