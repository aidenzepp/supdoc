//! End-to-end parity with docref: runs the built `supdoc` binary against copies of the fixtures in
//! `tests/fixtures/` and checks what lands in `supdoc.lock`, the exit codes and the reports.
//!
//! These are docref's 11 fixtures and 60 integration tests (`docref/tests/integration.rs`), adjusted to
//! supdoc's decisions (planning 01–06 and the approved command specs):
//!
//! - `docref init` recorded the lockfile. supdoc's `init` writes only a starter config, and the first
//!   `supdoc update --yes` records the lockfile (05 C10), so tests record with `update --yes`.
//! - `.docref.toml` and `.docref.lock` are `supdoc.config.toml` and `supdoc.lock`. Patterns are globs
//!   without trailing slashes (01).
//! - A hash covers a symbol's exact bytes (03 R6), so reformatting inside a symbol fails its link,
//!   where docref normalized whitespace away. A comment above an item is outside it and still passes.
//! - docref's STALE is supdoc's `failed`. Both `failed` and `broken` exit 1 (05 C4); docref used 2 for
//!   broken.
//! - `update` narrows by the files links are written in, not by target (05 C9): docref's
//!   `update src/lib.rs#A` and `update --from docs/guide.md` both become `update docs/guide.md --yes`.
//!   Writes preview unless `--yes` (06 X4).
//! - `status` and `refs` are `list` and `list TARGET` (06a §5.4). `--format json` is `--json`, and passed
//!   links show in `check --json` only with `-v`.
//! - A Markdown dotted path starts at the top heading (`overview.foo.example`), and a duplicate heading
//!   answers to its numbered GitHub anchor (`example-1`) instead of being ambiguous (03 R12). The
//!   ambiguity test moved to a Rust method name two types share; the `scoped` fixture gained `Server`
//!   for it.
//! - docref's `wholefile_update_bare_path` (`update src/lib.rs` accepts the whole-file link while `add`
//!   stays stale) is `updating_the_file_with_the_whole_file_link_accepts_it_while_a_symbol_link_elsewhere_stays_failed`:
//!   with no narrowing by target, the two links sit in different docs and `update docs/guide.md --yes`
//!   accepts one.
//! - Not ported, because supdoc has no such command: `namespace remove` (2 tests), `namespace add`
//!   (1 test) and `watch` (2 tests, "Later" in 06).

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

#[cfg(test)]
mod commands;
#[cfg(test)]
mod containment;
#[cfg(test)]
mod json;
#[cfg(test)]
mod languages;
#[cfg(test)]
mod lifecycle;
#[cfg(test)]
mod list;
#[cfg(test)]
mod namespaces;
#[cfg(test)]
mod resolve;
