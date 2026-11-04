# supdoc

> *Eh… what's up, doc?*
>
> supdoc knows: it tells you when the code behind your docs changed.

Keep documentation in step with the code it describes.

supdoc tracks links from documentation to code, such as `[text](src/lib.rs#symbol)`. It warns you when the
code behind a link changes, so the docs can be updated with it.

> **Status:** v0.1, published on [crates.io](https://crates.io/crates/supdoc). Expect rough edges.

## Install and upgrade

```sh
cargo install supdoc --locked     # install
supdoc upgrade                    # preview the latest version; add --yes to install it
supdoc upgrade 0.3.0              # preview a specific version, a downgrade if it is older
```

`supdoc upgrade` works only for a cargo install from crates.io and refuses any other install, naming what it
sees.

## How it works

1. Link docs, or code comments, to code with ordinary Markdown links: `[add](src/lib.rs#add)` links one
   symbol, `[lib](src/lib.rs)` a whole file. Paths are relative to the file the link is in.
2. `supdoc update --yes` records a hash of every linked target in `supdoc.lock`. Commit it.
3. `supdoc check` compares today's code with those hashes. When a linked target changed, the check fails
   until someone rereads the doc and runs `supdoc update` for it.

Links in Rust, TypeScript, JavaScript, Python, Go, Bash and Markdown files are read. Markdown headings are
symbols too: `[setup](docs/guide.md#install)`.

## Commands

| Command | Does |
|---|---|
| `supdoc check [PATH]...` | Checks links against `supdoc.lock`; exits 1 when one needs attention (CI gate) |
| `supdoc update [PATH]...` | Records the current code's hashes in `supdoc.lock` |
| `supdoc upgrade [VERSION]` | Installs a newer (or older) supdoc with cargo; previews unless `--yes` |
| `supdoc fix [PATH]...` | Repairs broken links whose fix is certain, rewrites stale `from(...)` code blocks from their sources, and replaces unformatted code blocks with the formatter's output |
| `supdoc list [TARGET]` | Lists links and their states, such as who documents `src/lib.rs#add` |
| `supdoc resolve FILE [SYMBOL]` | Lists a file's symbols, or checks that one resolves |
| `supdoc init` | Writes a starter `supdoc.config.toml` |
| `supdoc config show` | Shows the config in effect |
| `supdoc namespace list`, `supdoc namespace rename OLD NEW` | Lists or renames namespaces |
| `supdoc schema [COMMAND]` | Prints the JSON Schema of a command's `--json` output |
| `supdoc completions SHELL` | Prints a shell completion script |
| `supdoc skill install` | Installs supdoc's agent skill |
| `supdoc trust [PATH]`, `supdoc trust --revoke [PATH]` | Trusts a checkout to run its code blocks' tools, or stops trusting it |

`update`, `fix`, `upgrade`, `namespace rename` and `skill install` preview their changes unless given `--yes`.
`init` writes immediately; use `--dry-run` to preview it. Every command accepts `--json` and `--jsonl`
for machine-readable output. Adding optional fields, or new values of an existing enum (states, actions, kinds,
JSONL record types), keeps `schema_version`; removing or renaming bumps it (currently 2, since 0.2), so consumers must tolerate
values they do not know. Exit codes: 0 clean, 1 failing or refused, 2 usage error, 70 supdoc bug, 74 I/O
error, 78 config or lockfile error.

## Configuration

supdoc works without a config. `supdoc init` writes a `supdoc.config.toml` with every key commented out:
`include` and `exclude` globs, `extends` to inherit another config, and `[namespaces]` to give a directory a
short name, so `auth:src/session.rs#login` works from any file. supdoc searches upward from the current
directory for the config, stopping at the top of the git work tree, and `-c` overrides that search. `supdoc.lock`
sits beside the config.

`[features.hash]` sets `line-endings`. The default, `"normalize"`, drops the CR of each CRLF before hashing, so
a checkout with Windows line endings still matches the lock. `"exact"` hashes every byte as is. `supdoc config
show` prints the value in effect and the file that set it.

Don't run supdoc on an untrusted PR after writing secrets into the checkout.

Threat model, for a checkout you don't trust:

- Outside the project root, supdoc reads only its own inputs: a `supdoc.config.toml` reached through
  `extends` or as a project config symlinked to an outside file of that name, whose lines can appear in
  config errors, and ignore files: those in the directories above the project, up to the top of its git
  work tree, plus the global excludes file, and a `.git/info/exclude` reached through a `.git` symlink,
  a symlinked `.git/info` or `info/exclude`, or a `.git` file's `gitdir` and `commondir`. To find the
  global excludes file it reads the git config files that can name it: `~/.gitconfig` and
  `$XDG_CONFIG_HOME/git/config` (or the file `GIT_CONFIG_GLOBAL` names) for the user, `/etc/gitconfig`
  (or the file `GIT_CONFIG_SYSTEM` names) for the system. It never quotes a rule line from an ignore file
  outside the root, or a line from a git config.
- It reads, hashes, lists symbols from and quotes no other file outside the root, linked files included.
  A formatter it runs on a code block may still read its own config files, as the next bullet says.
- It doesn't promise to hide whether a path outside the root exists. An out-and-back symlink can change
  which error or exit code appears. That is accepted: on ordinary `pull_request` CI, a PR can already run
  commands.
- **Network.** `supdoc upgrade` is supdoc's only network access: it runs `cargo`, which talks to crates.io.
  No other command touches the network, and no command checks for updates on its own.
- **Checking code blocks.** With a `[features.code-blocks]` table, `supdoc check` and `supdoc fix` check the
  code blocks in your docs with your project's own tools. What runs depends on whether you trust the checkout:
  - **Any checkout (the default):** supdoc runs only tools from its own table that read the block and plain
    config files: rustfmt, shfmt, ruff, black and shellcheck. Each block goes to the tool on standard input,
    so the tool opens no other source file. supdoc ignores a `rust-toolchain.toml` that names a path, never
    downloads a toolchain, never uses a tool installed inside the checkout, and skips Rust blocks that read
    files or environment variables at compile time (`include_str!`, `env!` and the like). Each tool still
    reads its own config files, which can sit outside the project, and its errors can quote them.
  - **A checkout you trust:** tools that compile or load code from the checkout belong to the project tier.
    clippy is the only one today, and it doesn't run yet even in a trusted checkout: it waits for a later
    audit of the scanner in front of it, and its step counts as `lint not run`. supdoc trusts a checkout only
    from outside it: run `supdoc trust` once (it records the directory of the config; `supdoc trust --revoke`
    undoes it, and a linked worktree of a trusted repository at the same path inside its work tree is
    trusted too), or set `SUPDOC_TRUST=1` for one run, as in a CI workflow. No file in the checkout can turn
    trust on. `tier = "static"` in `[features.code-blocks]` can only turn it off: it holds every step to
    the default list even in a trusted checkout, with no warning. `tier = "run"` is a config error until
    running examples exists.
  - A step held back because the checkout isn't trusted is counted `not run` with its reason, never as
    passed, and the run gets one `code-block-not-trusted` warning (exit 0; a failure under
    `--deny-warnings`) that names `supdoc trust`, `SUPDOC_TRUST=1` and the opt-outs.
  - supdoc doesn't check who owns a trusted directory or the trust store
    (`$XDG_STATE_HOME/supdoc/trusted-roots`, else `~/.local/state/supdoc/trusted-roots`). Anyone who can
    write that file can grant trust.
  - Trust follows the directory, not its contents: if you trust a checkout and then switch it to a branch
    you don't trust, the next check runs that branch's code once clippy is enabled. Never set
    `SUPDOC_TRUST` in a `pull_request_target` workflow, or in any job that has secrets and checks out a
    pull request's code.

### Differences from git

supdoc reads ignore files as git does, with these known exceptions:

- Rules match case-sensitively whatever `core.ignorecase` says, so a checkout scans the same on a
  case-insensitive laptop and on Linux CI.
- `core.excludesFile` is read only from the user and system git config, not from a repository's own config
  or the files it includes. Use `.git/info/exclude` for a per-repository exclude.
- A `.git` directory counts as a repository when it holds `HEAD`, `objects` and `refs`, as git checks.
  supdoc doesn't read `HEAD` to check that it names a commit, so a `.git` with a corrupt `HEAD` still marks
  the top of the work tree here, where git searches further up.
- Inside a nested repository or submodule, the global excludes file's anchored patterns still count from the
  project's work-tree top, not from the nested repository's own top as git run there counts them.
- supdoc also reads `.ignore` files, which git doesn't. They apply in the project root and the directories
  above it up to the top of the work tree, and beneath the root. A `.ignore` rule outranks every `.gitignore`
  rule, so a `!pattern` in a `.ignore` brings back a file a `.gitignore` hides.
  They still apply past a nested repository's top, where `.gitignore` rules stop.
- supdoc judges the files and directories beneath the project root, never the root itself or the directories
  above it. When a rule above the root ignores the root, or a directory on the way to it (`pkgs/` with the
  root at `pkgs/web`), git ignores every file beneath it and supdoc still checks them, as ripgrep does:
  running supdoc in a directory asks it to check that directory. A rule above the root that matches the
  files themselves, such as `*.gen.md`, still applies.
- supdoc applies the ignore rules inside the root to every file, whether or not git tracks it. It doesn't
  read git's index, and git doesn't ignore a tracked file.

Outside any git repository, with no `.git` in the root or above it, there is no work tree to stop at. supdoc
applies the project's own `.gitignore` and `.ignore` files and the global excludes file, and reads none above
the project root.

## Code blocks

supdoc can also keep the code blocks in your docs in step with the code. It's off until the config has a
`[features.code-blocks]` table; an empty table turns it on:

```toml
[features.code-blocks]
# include = ["docs/**"]   default: the top-level `include`
# exclude = []            default: the top-level `exclude`
```

`include` and `exclude` choose which files' code blocks are read, with the same globs as the top-level keys.
Each defaults to the top-level value, and setting one never changes which files links are read from.

Mark a block by writing attributes after its language, separated by spaces:

- **`from(PATH#SYMBOL)`**: the block is exactly that symbol's text. `PATH` is relative to the doc, and a
  dotted symbol names a member, such as `src/limits.rs#Limits.resolve`. Repeat it to show several symbols;
  the parts join in order with one blank line between them. `supdoc check` fails with `code-block-stale` when
  the block no longer matches, and `supdoc fix --yes` copies the current text in.
- **`uses(PATH#SYMBOL)`**: the block is written by hand but depends on that symbol, in any language. It's
  tracked like a link: `supdoc update --yes` records the symbol's hash in `supdoc.lock`, and `supdoc check`
  fails when the symbol changes, until you reread the block and run `update` again. It may repeat.

````markdown
```rust from(../src/limits.rs#Limits.resolve)
pub fn resolve(&self) -> u32 {
    self.max
}
```
````

Attributes are always parenthesized, never `key=value`: rustdoc rejects the `=` and then stops treating the
block as Rust, so it loses highlighting and drops out of `cargo test --doc`. A comma doesn't separate
attributes either (` ```rust,ignore ` is one unknown word); supdoc warns and names the spaced spelling.

supdoc checks what is written and never skips a block on its own. To exclude a block, mark it and say why:

- `ignore(reason)`: check nothing in this block. rustdoc reads it as its own `ignore` too.
- `format(off: reason)` and `lint(off: reason)`: skip only formatting or only linting. The reason runs
  to the matching `)`, so it may hold spaces and commas.
- `format(full)` and `lint(full)`: run the step on this block even though its tool's table turns it off.
  A Rust block's lint doesn't run yet: clippy needs `supdoc trust` and a later audit.

A block attribute beats the tool's table for that block only. The selected tool accepts `full` and `off`;
any other value, or a step attribute on a block whose language has no tool for that step, warns
`code-block-unknown-attribute`. An `ignore` or `off` without a reason still takes effect, with a
`code-block-missing-reason` warning.

A language table turns on its formatter: with `[features.code-blocks.shell]`, shell blocks go through
`shfmt`, found on `PATH`. A block the formatter would change fails with `code-block-unformatted`, and
`supdoc fix --yes` replaces its body with the formatter's output. supdoc refuses that fix, and the fix
for a stale `from(...)` block, with a `code-block-fix-unsafe` warning when the new body could end the
block early: it holds a control character other than a tab or a line ending, a line of it would close
the fence, or the doc, parsed again with the fix applied, no longer holds the same fenced blocks.

A Rust block that uses a name it doesn't define fails with `code-block-missing-name`, once per name at the
line that first uses it; its other lint diagnostics are dropped, since they follow from the missing name.

The language table selects tools and holds the language's own settings; each tool's options live in the
tool's own table:

```toml
[features.code-blocks.python]
formatter = "black"      # default: ruff-format
linter = "ruff"          # the only Python linter
[features.code-blocks.python.ruff]
lint = "off"             # "full" (the default) or "off"; a TOML comment is the place for why
[features.code-blocks.rust]
edition = "2021"         # Rust also takes `edition`; shell takes `dialect` ("bash", "posix", "mksh")
```

A table for a tool the language doesn't select, or a key or value its tool doesn't accept, is a config
error. A step a config table sets to `"off"` counts as `format off` or `lint off` in the summary, and one an
author marks `format(off: ...)` or `lint(off: ...)` counts as `format skipped` or `lint skipped`; a step that was due but
didn't run, such as clippy's, which is project tier, counts as `lint not run` with its reason.

A Rust snippet that needs surrounding code to make sense takes one of three explicit forms:

1. **Show the real code.** If it's the project's own code, make it a `from(...)` block, so it can't drift.
2. **Write the context into the block** as ordinary, visible code (`use std::time::Instant;`,
   `fn main() {` … `}`). Every line is checked as written.
3. **Keep it partial and say so**: `lint(off: needs crate context)`, or `ignore(placeholder)` for a block
   that doesn't parse on its own.

Most renderers hide the attributes: GitHub, markdown-it, Docusaurus, mdBook and TypeDoc show only the
highlighted code. Mintlify shows any text after the language as the block's title, so marked blocks there
carry their attributes as a header.

## Agent skill

[`SKILL.md`](SKILL.md) teaches coding agents to use supdoc: find the docs behind code before changing it, and
reread them before accepting a change. It's embedded in the binary, so it always matches your version.

```bash
supdoc skill install --yes            # into this project
supdoc skill install --global --yes   # into your home directory
```

Each writes two copies: `.agents/skills/supdoc/SKILL.md` for agents that read the shared directory, and
`.claude/skills/supdoc/SKILL.md` for Claude Code. With `--global` they go under `$HOME`, and Claude Code's
copy under `$CLAUDE_CONFIG_DIR` when it's set. Without `--yes`, the command shows what it would write. An
existing `SKILL.md` with different contents is replaced only with `--yes`. A project install writes nothing
outside the project: if `.agents` or `.claude` is a symlink that leads out of it, the command names the link
and refuses.

## Development

```bash
cargo fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```
