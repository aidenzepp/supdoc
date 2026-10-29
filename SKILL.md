---
name: supdoc
description: Keep docs in step with code using the supdoc CLI. Use before changing code that docs link to, after such a change, when `supdoc check` fails, or when writing docs that should point at code.
---

# supdoc

supdoc tracks Markdown links from docs and code comments to code, such as
`[add](src/lib.rs#add)`. `supdoc.lock` records a hash of each linked target. When the code behind a
link changes, `supdoc check` fails until someone rereads the doc and confirms it with
`supdoc update`.

## Writing links

- `[text](src/lib.rs)` links a whole file. `[text](src/lib.rs#add)` links one symbol.
- Nested symbols use dots: `#Config.validate`. Markdown headings are symbols too:
  `docs/guide.md#install`.
- Paths are relative to the file the link is written in. A leading `/` means the project root.
- `name:path#symbol` uses a namespace from `supdoc.config.toml`, such as `auth:src/session.rs#login`.
- Links in code comments work the same way.
- Before writing a symbol link, confirm it resolves: `supdoc resolve src/lib.rs add`. Without a
  symbol, `supdoc resolve FILE` lists every symbol the file exposes.

## Link states

| State | Meaning | What to do |
|---|---|---|
| `passed` | The code matches the lockfile | Nothing |
| `failed` | The code changed since the doc was confirmed | Reread the doc, edit it if needed, then `supdoc update PATH --yes` |
| `broken` | The file, symbol or namespace can't be found | `supdoc fix`, or edit the link by hand |
| `new` | The link has no lockfile entry yet | Confirm the doc is accurate, then `supdoc update PATH --yes` records it |
| `obsolete` | A lockfile entry whose link is gone | `supdoc update --yes` removes it |

## Workflow

1. Before changing code, find the docs that describe it: `supdoc list src/lib.rs#add`.
2. Change the code, then run `supdoc check`.
3. For each `failed` link, read the doc at the reported line and fix anything the change made
   wrong. Only then accept it: `supdoc update docs/guide.md --yes`.
4. For `broken` links, preview `supdoc fix`, then apply it with `supdoc fix --yes`. It rewrites a
   link only when the repair is certain. Fix the rest by hand.

Never run `supdoc update --yes` just to make `check` pass. Accepting a `failed` link says the doc
is still accurate.

## Commands

| Command | Does |
|---|---|
| `supdoc check [PATH]...` | Checks links against `supdoc.lock`. Exits 1 on any `failed`, `broken` or `new` link; `--deny-warnings` also fails on `obsolete` |
| `supdoc list [TARGET] [--state S]` | Lists links and their states, optionally only those to one target |
| `supdoc resolve FILE [SYMBOL]` | Lists a file's symbols, or checks that one resolves |
| `supdoc update [PATH]... [--state S]` | Records hashes into `supdoc.lock` |
| `supdoc fix [PATH]...` | Repairs broken links whose fix is certain |
| `supdoc namespace rename OLD NEW` | Renames a namespace in the config, the lockfile and every link |
| `supdoc init` | Writes a starter `supdoc.config.toml` |
| `supdoc config show`, `supdoc namespace list` | Show the config in effect |
| `supdoc schema [COMMAND]` | Prints the JSON Schema of a command's `--json` output |

Commands that write (`update`, `fix`, `namespace rename`, `skill install`) only preview unless given
`--yes`. `--dry-run` always previews.

## Machine-readable output

- `--json` prints one JSON document: `schema_version`, `command`, `ok`, `root`, `data`, `findings`,
  `truncated`. Errors are JSON too, on stderr.
- `--jsonl` prints one object per line, ending with a summary.
- `supdoc schema check` describes the shape of `supdoc check --json`.
- Paths in JSON are relative to the project root, the directory holding `supdoc.config.toml`.

Exit codes: 0 clean, 1 failing or refused, 2 usage error, 70 supdoc bug, 74 I/O error, 78 config or
lockfile error.
