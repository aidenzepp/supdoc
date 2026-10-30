# supdoc

Keep documentation in step with the code it describes.

supdoc tracks links from documentation to code, such as `[text](src/lib.rs#symbol)`. It warns you when the
code behind a link changes, so the docs can be updated with it.

> **Status:** early development, ahead of the first release.

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
| `supdoc fix [PATH]...` | Repairs broken links whose fix is certain |
| `supdoc list [TARGET]` | Lists links and their states, such as who documents `src/lib.rs#add` |
| `supdoc resolve FILE [SYMBOL]` | Lists a file's symbols, or checks that one resolves |
| `supdoc init` | Writes a starter `supdoc.config.toml` |
| `supdoc config show` | Shows the config in effect |
| `supdoc namespace list`, `supdoc namespace rename OLD NEW` | Lists or renames namespaces |
| `supdoc schema [COMMAND]` | Prints the JSON Schema of a command's `--json` output |
| `supdoc completions SHELL` | Prints a shell completion script |
| `supdoc skill install` | Installs supdoc's agent skill |

`update`, `fix`, `namespace rename` and `skill install` preview their changes unless given `--yes`.
`init` writes immediately; use `--dry-run` to preview it. Every command accepts `--json` and `--jsonl`
for machine-readable output. Adding optional fields, or new values of an existing enum (states, actions, kinds,
JSONL record types), keeps `schema_version`; removing or renaming bumps it, so consumers must tolerate values they
do not know. Exit codes: 0 clean, 1 failing or refused, 2 usage error, 70 supdoc bug, 74 I/O
error, 78 config or lockfile error.

## Configuration

supdoc works without a config. `supdoc init` writes a `supdoc.config.toml` with every key commented out:
`include` and `exclude` globs, `extends` to inherit another config, and `[namespaces]` to give a directory a
short name, so `auth:src/session.rs#login` works from any file. supdoc searches upward from the current
directory for the config, and `supdoc.lock` sits beside it.

`[features.hash]` sets `line-endings`. The default, `"normalize"`, drops the CR of each CRLF before hashing, so
a checkout with Windows line endings still matches the lock. `"exact"` hashes every byte as is. `supdoc config
show` prints the value in effect and the file that set it.

Don't run supdoc on an untrusted PR after writing secrets into the checkout.

Threat model, for a checkout you don't trust:

- Outside the project root, supdoc reads only its own inputs: a `supdoc.config.toml` reached through
  `extends` or as a project config symlinked to an outside file of that name, whose lines can appear in
  config errors, and ignore files: those in the directories above the project, up to the top of its git
  work tree, plus the global excludes file, and a `.git/info/exclude` reached through a `.git` symlink,
  a symlinked `.git/info` or `info/exclude`, or a `.git` file's `gitdir` and `commondir`. It never quotes
  a rule line from an ignore file outside the root.
- It reads, hashes, lists symbols from and quotes no other file outside the root, linked files included.
- It doesn't promise to hide whether a path outside the root exists. An out-and-back symlink can change
  which error or exit code appears. That is accepted: on ordinary `pull_request` CI, a PR can already run
  commands.

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
