# supdoc

*Eh… what's up, doc?*

supdoc tells you when the code behind your docs changed.

It reads Markdown links to code, such as `[add](src/lib.rs#add)`, from your docs and from code comments
in Rust, TypeScript, JavaScript, Python, Go and Bash. It records a hash of each linked file, symbol,
Markdown heading or JSON, YAML or TOML key in `supdoc.lock`, and `supdoc check` fails when one
changes. It doesn't judge whether a doc is still right. A person rereads it and runs `supdoc update`.
supdoc can also check the code blocks in your docs. It is pre-1.0 and published on
[crates.io](https://crates.io/crates/supdoc).

## Install

```bash
cargo install supdoc --locked
```

From a checkout:

```bash
cargo install --path crates/supdoc --locked
```

`supdoc upgrade --yes` installs the latest release. It works only for a `cargo install` from crates.io.

## Start

In a git repository with a doc that links to code:

```console
$ cat docs/guide.md
# Guide

[`add`](../src/lib.rs#add) returns the sum of two numbers.
$ supdoc init
Wrote supdoc.config.toml
$ supdoc update --yes
record  docs/guide.md:3  ->  src/lib.rs#add  new
supdoc.lock: 1 recorded
$ supdoc check
Links  1 passed
```

After a change to the body of `add`, the check fails until someone rereads the doc and accepts it:

```console
$ supdoc check
error[link-failed]: `src/lib.rs#add` changed since this doc was confirmed
 --> docs/guide.md:3:1
  |
3 | [`add`](../src/lib.rs#add) returns the sum of two numbers.
  | ^^^^^^^^^^^^^^^^^^^^^^^^^^
  |
  = help: reread this, and if it's still accurate run `supdoc update docs/guide.md --yes`

Links  1 failed
$ supdoc update docs/guide.md --yes
accept  docs/guide.md:3  ->  src/lib.rs#add  failed -> passed
supdoc.lock: 1 accepted
```

Commit `supdoc.config.toml` and `supdoc.lock`, and run `supdoc check` in CI.

## Commands

| Command | Writes | Does |
| --- | --- | --- |
| `check [PATH]...` | its cache | Checks links and code blocks; exits 1 when one needs attention |
| `update [PATH]...` | `supdoc.lock` | Records the current code's hashes |
| `fix [PATH]...` | docs, comments, its cache | Repairs links whose fix is certain, and stale or unformatted code blocks |
| `list [TARGET]` | no | Lists links and their states, such as who documents `src/lib.rs#add` |
| `resolve FILE [SYMBOL]` | no | Lists a file's symbols, or checks that one resolves |
| `init` | config | Writes a starter `supdoc.config.toml`; refuses to overwrite |
| `config show` | no | Shows the config in effect and the file each value came from |
| `namespace list`, `namespace rename OLD NEW` | rename: config, lock, links | Lists or renames namespaces |
| `schema [COMMAND]` | no | Prints the JSON Schema of a command's `--json` output |
| `completions SHELL` | no | Prints a shell completion script |
| `skill install` | skill files | Installs supdoc's agent skill |
| `upgrade [VERSION]` | the binary | Installs a newer or older supdoc with cargo |
| `trust [PATH]` | the trust store | Trusts a checkout to run code-block tools that load its code; `--revoke` undoes it |

`update`, `fix`, `namespace rename`, `skill install` and `upgrade` preview unless given `--yes`. `init`
writes at once, and `--dry-run` previews it. Every command accepts `--json` and `--jsonl`. Exit codes: 0
clean, 1 failing or refused, 2 usage error, 70 supdoc bug, 74 I/O error, 78 config or lockfile error.
Each command's `--help` is the reference for its flags. No command writes in the project beyond what the
table names. With code blocks on, `check` and `fix` keep a result cache, and a trusted build its output,
under `~/.cache/supdoc` (or `$XDG_CACHE_HOME/supdoc`).

## Configuration

supdoc works without a config. `supdoc init` writes this, with every key commented out:

```toml
# supdoc.config.toml. Every key is optional.

# Files to scan for links. Globs, relative to this file's directory.
# Each pattern also matches everything beneath it. The default is everything.
# include = ["**"]

# Removed from include. Exclude always wins.
# exclude = []

# Another config to inherit from, relative to this file.
# extends = "../../supdoc.config.toml"

# Short names for directories, relative to this file.
# [namespaces]
# auth = "services/auth"

# How files are hashed. "normalize" drops the CR of each CRLF, so a checkout with other line
# endings still matches the lock. "exact" hashes every byte as is.
# [features.hash]
# line-endings = "normalize"
```

supdoc searches for the config from the current directory up to the top of the git work tree, and `-c`
names one instead. `supdoc.lock` sits beside it. supdoc reads the files your `.gitignore`, `.ignore` and git
exclude files leave, with a few differences from git that `supdoc check --help` lists.

A `[features.code-blocks]` table, even an empty one, turns on code-block checking. A block marked
`from(PATH#SYMBOL)` must stay a copy of that symbol. A block marked `uses(PATH#SYMBOL)` is tracked
like a link. A language's table, such as `[features.code-blocks.rust]`, turns on that language's
formatter and linter. `supdoc check --help` lists every tool, attribute and how to opt a block out.

## Safety

- Outside the project root, supdoc reads only its own inputs: its config chain, ignore files, the git
  config that names the global excludes file, a linked worktree's `.git` and `commondir`, the trust
  store, its result cache, and, for `upgrade`, cargo's `.crates2.json`. It never reads, hashes or quotes
  a linked file outside the root, and never quotes an ignore rule or git config line from there.
- `supdoc upgrade` is the only command that uses the network. Nothing checks for updates on its own.
- Each write supdoc makes is checked to land inside the directory that owns it, just before it
  happens, and an ignore file that is a symlink isn't read. Both checks hold when they are made: a
  file or directory swapped for a symlink while supdoc runs can still send a write outside, or make
  supdoc read one ignore file from outside the root.
- In a checkout you don't trust, code-block tools come from supdoc's own list, found on `PATH` but
  never inside the checkout, and read only the block and plain config files.
- A `rust-toolchain.toml` that names a path leaves Rust blocks unchecked, and supdoc never downloads a
  toolchain.
- A Rust block that reads a file or an environment variable at compile time (`include_str!`, `env!`)
  isn't checked.
- Some tools run the project's own code, so they run only in a checkout you trust. clippy builds your
  project and runs its build scripts and proc macros. pylint imports the blocks' modules, prettier and
  eslint load the project's config and plugins, and each is found in the project's `.venv/bin` or
  `node_modules/.bin` before `PATH`. Custom formatter and linter commands written in the config, or in
  a config it extends, run only in a checkout you trust, as pylint and clippy do.
- A checkout's `[limits]` can only lower supdoc's ceilings unless you trust it.
- `supdoc trust` records trust, and `SUPDOC_TRUST=1` grants it for one run. No file in the checkout
  can grant it. supdoc doesn't check who owns the trust store, so anyone who can write it can grant
  trust. What trust would have run is named in one `code-block-not-trusted` warning per run: exit 0,
  but a failure under `--deny-warnings`.
- Never set `SUPDOC_TRUST` in a CI job that has secrets and checks out a pull request's code. Don't run
  supdoc on an untrusted pull request after writing secrets into the checkout. `supdoc trust --help`
  covers worktrees and CI runners.

## Agent skill

[`SKILL.md`](SKILL.md) teaches coding agents to find the docs behind code before changing it. The binary
embeds it, so it matches your version. `supdoc skill install --yes` writes it into the project, and
`--global` into your home directory.

## Development

Requires stable Rust (`rust-toolchain.toml`). A change passes:

```bash
cargo fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```

CI pins the formatters and linters the code-block tests run in `ci/`. Coding agents start at
`AGENTS.md`.

## License

MIT
