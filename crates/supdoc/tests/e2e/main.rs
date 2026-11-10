//! End-to-end tests: run the built `supdoc` binary against copies of the fixtures in
//! `tests/fixtures/` and checks what lands in `supdoc.lock`, the exit codes and the reports.
//!
//! The modules cover: `commands`, `helps` and `json` (command surface and output), `lifecycle`
//! (init, update, check), `containment`, `resolve`, `paths`, `ignore`, `namespaces` and `languages`
//! (what links resolve to), `list` and `list_containment` (the `list` command), `limits`
//! (the size, timeout and similar limits), and `code_blocks`, `code_blocks_fix`,
//! `code_blocks_fragment`, `code_blocks_md`, `code_blocks_python`, `code_blocks_rust` and `code_blocks_ts` (checking the code blocks in docs), and `trust` (the
//! trust store `supdoc trust` writes).
//!
//! The suite began as a port of docref's integration tests (`docref/tests/integration.rs`), adjusted
//! to supdoc's decisions:
//!
//! - `docref init` recorded the lockfile. supdoc's `init` writes only a starter config, and the first
//!   `supdoc update --yes` records the lockfile, so tests record with `update --yes`.
//! - `.docref.toml` and `.docref.lock` are `supdoc.config.toml` and `supdoc.lock`. Patterns are globs
//!   without trailing slashes.
//! - A hash covers a symbol's exact bytes, so reformatting inside a symbol fails its link,
//!   where docref normalized whitespace away. A comment above an item is outside it and still passes.
//! - docref's STALE is supdoc's `failed`. Both `failed` and `broken` exit 1; docref used 2 for
//!   broken.
//! - `update` narrows by the files links are written in, not by target: docref's
//!   `update src/lib.rs#A` and `update --from docs/guide.md` both become `update docs/guide.md --yes`.
//!   Writes preview unless `--yes`.
//! - `status` and `refs` are `list` and `list TARGET`. `--format json` is `--json`, and passed
//!   links show in `check --json` only with `-v`.
//! - A Markdown dotted path starts at the top heading (`overview.foo.example`), and a duplicate heading
//!   answers to its numbered GitHub anchor (`example-1`) instead of being ambiguous. The
//!   ambiguity test moved to a Rust method name two types share; the `scoped` fixture gained `Server`
//!   for it.
//! - docref's `wholefile_update_bare_path` (`update src/lib.rs` accepts the whole-file link while `add`
//!   stays stale) is `updating_the_file_with_the_whole_file_link_accepts_it_while_a_symbol_link_elsewhere_stays_failed`:
//!   with no narrowing by target, the two links sit in different docs and `update docs/guide.md --yes`
//!   accepts one.
//! - Not ported, because supdoc has no such command: `namespace remove` (2 tests), `namespace add`
//!   (1 test) and `watch` (2 tests, still planned).
//!
//! Unix only: the suite uses Unix permissions, symlinks and `sh`. Windows is untested, and CI runs
//! only on Linux.

#![cfg(unix)]
#![cfg_attr(
    test,
    allow(
        clippy::missing_errors_doc,
        clippy::missing_panics_doc,
        reason = "tests report failures rather than document callable contracts"
    )
)]

#[cfg(test)]
mod support;

/// Whether mode `000` keeps this user out of a directory. It doesn't for root, where the
/// permission tests can't happen: each of them prints a skip reason and returns early.
#[cfg(test)]
fn permissions_restrict_this_user(test: &str) -> bool {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("the probe directory should be creatable");
    let file = dir.path().join("probe");
    std::fs::write(&file, "").expect("the probe file should be writable");
    let locked = std::fs::Permissions::from_mode(0o000);
    std::fs::set_permissions(dir.path(), locked).expect("the probe directory should be lockable");
    let restricted = std::fs::symlink_metadata(&file).is_err();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
        .expect("the probe directory should be unlockable");
    if !restricted {
        // Written to the process's stderr directly, which `cargo test` shows even for a passing test.
        let _ignored = writeln!(
            std::io::stderr(),
            "SKIPPED {test}: mode 000 does not restrict this user (running as root?)"
        );
    }
    restricted
}

#[cfg(test)]
mod code_blocks;
#[cfg(test)]
mod code_blocks_css;
#[cfg(test)]
mod code_blocks_custom;
#[cfg(test)]
mod code_blocks_fix;
mod code_blocks_fragment;
#[cfg(test)]
mod code_blocks_go;
#[cfg(test)]
mod code_blocks_json;
#[cfg(test)]
mod code_blocks_md;
#[cfg(test)]
mod code_blocks_python;
#[cfg(test)]
mod code_blocks_rust;
#[cfg(test)]
mod code_blocks_toml;
#[cfg(test)]
mod code_blocks_ts;
#[cfg(test)]
mod code_blocks_yaml;
#[cfg(test)]
mod commands;
#[cfg(test)]
mod containment;
#[cfg(test)]
mod helps;
#[cfg(test)]
mod ignore;
#[cfg(test)]
mod json;
#[cfg(test)]
mod languages;
#[cfg(test)]
mod lifecycle;
#[cfg(test)]
mod limits;
#[cfg(test)]
mod list;
#[cfg(test)]
mod list_containment;
#[cfg(test)]
mod namespaces;
#[cfg(test)]
mod path_hardening;
#[cfg(test)]
mod paths;
#[cfg(test)]
mod resolve;
#[cfg(test)]
mod trust;
#[cfg(test)]
mod update_version;
#[cfg(test)]
mod upgrade;
