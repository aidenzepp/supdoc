//! `supdoc list TARGET#symbol` reads no file outside the project root and never blocks on a file
//! that isn't a regular file (#60).

use std::fs;
use std::os::unix::fs::symlink;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::support::Project;

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

/// What a bounded run printed and how it exited.
struct Finished {
    /// The exit code, `None` if a signal ended it.
    code: Option<i32>,
    /// Everything on stdout.
    stdout: String,
    /// Everything on stderr.
    stderr: String,
}

/// Runs `supdoc args` from `services/web`, failing the test if it runs past [`HANG_LIMIT`].
fn run_bounded(project: &Project, args: &[&str]) -> Finished {
    let mut child = Command::new(env!("CARGO_BIN_EXE_supdoc"))
        .args(args)
        .current_dir(project.path("services/web"))
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the supdoc binary should start");
    let started = Instant::now();
    loop {
        if child
            .try_wait()
            .expect("the run should be waitable")
            .is_some()
        {
            break;
        }
        if started.elapsed() > HANG_LIMIT {
            drop(child.kill());
            drop(child.wait());
            panic!("`supdoc {}` hung for over {HANG_LIMIT:?}", args.join(" "));
        }
        thread::sleep(Duration::from_millis(50));
    }
    let output = child
        .wait_with_output()
        .expect("the finished run's output should be readable");
    Finished {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("stdout should be UTF-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr should be UTF-8"),
    }
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
    assert!(
        fifo.code.is_some(),
        "`list src/pipe.rs#x` was killed by a signal: {}",
        fifo.stderr
    );
}
