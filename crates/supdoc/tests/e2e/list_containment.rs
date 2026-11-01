//! `supdoc list TARGET#symbol` reads no file outside the project root and never blocks on a file
//! that isn't a regular file (#60).

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::process::Command;
use std::time::Duration;

use crate::support::{Project, Run};

/// How long one run may take before it counts as hung.
const HANG_LIMIT: Duration = Duration::from_secs(30);

/// The web service of the monorepo fixture, beside an outside file that defines `Config.new`.
///
/// `src/evil.rs` is an in-root symlink to that file, `src/pipe.rs` is a FIFO, and a doc links to
/// both. The `docs` namespace makes `docs:../…` a typed target.
fn project_with_outside_and_fifo_targets() -> Project {
    let project = Project::fixture("monorepo");
    fs::write(
        project.path("services/web/supdoc.config.toml"),
        "[namespaces]\ndocs = \"docs\"\n",
    )
    .expect("the config should be writable");
    fs::create_dir(project.path("services/outside"))
        .expect("the outside directory should be creatable");
    fs::write(
        project.path("services/outside/s.rs"),
        "struct Config;\nimpl Config {\n    fn new() {}\n}\n",
    )
    .expect("the outside file should be writable");
    symlink(
        "../../outside/s.rs",
        project.path("services/web/src/evil.rs"),
    )
    .expect("the symlink should be creatable");
    let made = Command::new("mkfifo")
        .arg(project.path("services/web/src/pipe.rs"))
        .status()
        .expect("mkfifo should start");
    assert!(made.success(), "mkfifo should create src/pipe.rs");
    fs::write(
        project.path("services/web/docs/probe.md"),
        "# Probe\n[a](../src/evil.rs#Config.new) and [p](../src/pipe.rs#x).\n",
    )
    .expect("the probe doc should be writable");
    project
}

/// Runs `supdoc args` from `services/web`, failing the test if it runs past [`HANG_LIMIT`].
fn run_bounded(project: &Project, args: &[&str]) -> Run {
    project.supdoc_bounded("services/web", args, HANG_LIMIT)
}

#[test]
fn list_target_symbol_never_reads_outside_the_root_or_blocks_on_a_fifo() {
    let project = project_with_outside_and_fifo_targets();

    // Symlink out: `#new` reaches `Config.new` only through the outside file's symbols. The target
    // is refused as outside the project, the same way whatever the outside file holds.
    let outside = run_bounded(&project, &["list", "src/evil.rs#new"]);
    assert_eq!(outside.code, Some(2), "{}", outside.stderr);
    assert_eq!(outside.stdout, "", "a refused run prints no report");
    assert!(
        outside.stderr.contains("cli-path-outside-project") && !outside.stderr.contains("Config"),
        "`list src/evil.rs#new` should be refused as outside the project: {}",
        outside.stderr
    );

    // `ns:../`: refused at parse, before anything is opened.
    let climbing = run_bounded(&project, &["list", "docs:../../outside/s.rs#new"]);
    assert_eq!(climbing.code, Some(2), "{}", climbing.stderr);
    assert!(
        climbing.stderr.contains("cli-target-invalid")
            && climbing.stderr.contains("climbs out of the project root"),
        "{}",
        climbing.stderr
    );

    // FIFO: never opened, so the run finishes.
    let fifo = run_bounded(&project, &["list", "src/pipe.rs#x"]);
    assert_eq!(
        fifo.code,
        Some(0),
        "`list src/pipe.rs#x` should list nothing and exit 0, never open the pipe: {}",
        fifo.stderr
    );
}
