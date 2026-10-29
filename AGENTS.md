# supdoc

supdoc is a Rust CLI that tracks links from documentation to code and reports when the referenced code changes. It is the successor to `docref`, rebuilt on a stable Rust workspace. Its config is `supdoc.config.toml` and its lockfile is `supdoc.lock`.

Create only the crates the project needs under `crates/`; do not pre-create shared or FFI crates.

If `.notes/` exists locally, it holds the maintainer's planned features and changes. It is gitignored; read it for intent, never commit it.

## Policy

`Cargo.toml` and `clippy.toml` are the source of truth.

- Fix code instead of weakening policy. Do not change lint levels, add broad `allow` attributes, or skip verification targets without explicit approval.
- Treat warnings as design feedback. They may remain while exploring, but resolve them before calling work complete.
- Keep workspace lint inheritance in every ordinary crate. Current Cargo adds it when creating a crate in this workspace.
- Give private production items the same documentation scrutiny as public items.
- Tests may omit callable-contract documentation, but they remain subject to production correctness and safety rules. `expect` requires a useful failure message; `unwrap` remains disallowed.
- Unsafe Rust is forbidden by default. Add an FFI boundary only when the project actually requires one and the exception has been approved.

If a rule appears inappropriate, explain the failing rule, why changing the code would be worse, the smallest policy change, and its consequences. Change the policy only after approval.

## Workflow

Create crates with `cargo new crates/<name>`. Keep the generated workspace lint inheritance:

```toml
[lints]
workspace = true
```

Add this attribute to each crate root and integration-test root:

```rust
#![cfg_attr(
    test,
    allow(
        clippy::missing_errors_doc,
        clippy::missing_panics_doc,
        reason = "tests report failures rather than document callable contracts"
    )
)]
```

The workspace has no build targets until its first crate exists.

Before completion, run:

```bash
cargo fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```

Resolve all warnings before finishing.

## Skills

- `.agents/skills/conventional-commits`: commit decisions, boundaries, staging, and messages.
- `.agents/skills/designing-trustworthy-functions-and-data`: functions, domain types, data structures, parsing boundaries, dependencies, effects, invariants, and abstraction levels.
- `.agents/skills/error-messages`: user-facing and developer-facing error messages.
- `.agents/skills/self-explanatory-tests`: test design, organization, failure messages, and debugging.
- `.agents/skills/unsafe-code`: unsafe Rust, FFI, raw pointers, syscalls, and `extern` blocks.

Keep only broadly reusable foundation skills here. Project-specific skills belong in the project that needs them.
