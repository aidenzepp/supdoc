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
for machine-readable output. Exit codes: 0 clean, 1 failing or refused, 2 usage error, 70 supdoc bug, 74 I/O
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
existing `SKILL.md` with different contents is replaced only with `--yes`.

## Development

```bash
cargo fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```
