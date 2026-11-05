//! The long help each command shows under `--help`, above its flags.
//!
//! `-h` shows the one-line summary from the command's doc comment; `--help` shows these texts.
//! Each one starts with that same summary line, so the two never disagree about what the command
//! is. These texts are the reference for behavior a user sees: the README points here.

/// `supdoc check --help`: the gate, the files it reads, and how it checks code blocks.
pub(crate) const CHECK_LONG_ABOUT: &str = "\
Check links against supdoc.lock (read-only)

Exits 1 when a link or a code block needs attention: a linked target changed since the lock \
recorded it, a link no longer resolves or has no lock entry yet, or a code block is stale, \
unformatted, or fails its format or lint step. With --deny-warnings, warnings also fail, such as an obsolete \
lock entry or a code-block-not-trusted warning. check writes nothing in the project. With code \
blocks on, it keeps a result cache in $XDG_CACHE_HOME/supdoc/code-blocks (else \
~/.cache/supdoc/code-blocks).

FILES

check reads the files your .gitignore, .ignore and git exclude files leave, as git does, with \
these differences:

  - Rules match case-sensitively, whatever core.ignorecase says.
  - core.excludesFile is read only from the user and system git config, never from a \
repository's own config.
  - A .ignore rule outranks every .gitignore rule, and .ignore files still apply past a nested \
repository's top, where .gitignore rules stop.
  - A rule above the project root that ignores the root, or a directory on the way to it, isn't \
applied: running supdoc in a directory checks it. A rule above the root that matches the files \
themselves, such as *.gen.md, still applies.
  - A rule ignores a file even when git tracks it. git's index isn't read.
  - Outside any git repository, no ignore file above the root is read. The global excludes file \
still applies.

Outside the root, supdoc reads only its own inputs: its config chain, ignore files, the git config \
that names the global excludes file, the .git and commondir of a linked worktree, the trust \
store and the result cache. It never quotes an ignore rule or git config line from there. \
It doesn't hide whether a path outside the root exists: an out-and-back symlink can change which \
error or exit code appears.

DATA LINKS

JSON, YAML and TOML files are link targets, never scanned for links. A link names a key by its \
path, such as package.json#scripts.test or ci.yml#jobs.test.steps.1:

  - `.` joins keys from the root, and a number names an array element, counting from 0.
  - Write `~` in a key as `~0` and `.` as `~1`, so a key a.b is a~1b. The fragment is \
percent-decoded first, so write a key's % as %25.
  - A YAML file of several non-empty documents names each by its index first, such as \
deploy.yaml#1.spec.replicas. An alias is never expanded.
  - Two keys with the same path make a link to it ambiguous.

A data file whose key paths hold more segments than max-key-segments, or more text than \
max-key-path-size, fails read-too-many-keys or read-key-paths-too-large. A package-lock.json over \
about 3 MB exceeds the default max-key-segments: raise it under [limits], with \
SUPDOC_MAX_KEY_SEGMENTS or with --max-key-segments. A YAML file past 32,768 lines, where a \\r\\n \
counts as two, fails read-too-many-lines, and one nested past column 100 fails read-too-deep; no \
setting raises either.

CODE BLOCKS

Code blocks are checked only when the config has a [features.code-blocks] table; an empty table \
turns it on. Its include and exclude keys choose which files' blocks are read, default to the \
top-level keys, and never change which files links are read from. A language's table, such as \
[features.code-blocks.rust], turns that language on and holds these keys:

  rust          formatter rustfmt, linter clippy, edition \"2015\" to \"2024\"
  shell         formatter shfmt, linter shellcheck, dialect \"bash\", \"posix\" or \"mksh\"
  python        formatter \"ruff-format\" (the default) or \"black\", linter ruff
  typescript    formatter biome-format, linter biome-lint
  javascript    formatter biome-format, linter biome-lint
  json          formatter biome-format, linter biome-lint; tag a block with comments jsonc
  yaml          formatter yamlfmt, linter yamllint
  toml          formatter tombi-format, linter tombi-lint
  markdown      formatter rumdl-format, linter rumdl
  go            formatter gofmt, no linter yet; tag a block go or golang

Each tool's options live in its own table, such as [features.code-blocks.python.ruff] with \
lint = \"off\". A table for a tool the language doesn't select is a config error.

Each tool reads the block on standard input and its own config files, which can sit outside the \
project; its errors can quote them. rumdl's config is found from the doc's directory up to the \
nearest .git inside the root, never above the root; yamlfmt's and yamllint's up to the root.

A snippet lacks the code around it, so some rules are off for blocks: biome lint skips \
noUnusedVariables, noUnusedImports and noUnusedFunctionParameters; rumdl turns off MD041, MD051 \
and MD057; yamllint turns off document-start; ruff drops F706, F704 and PLE1142 for a \
fragment(statements) block.

Write attributes after a block's language, separated by spaces:

  from(PATH#SYMBOL)
      The block is exactly that symbol's text. PATH is relative to the doc, and a dotted symbol \
names a member, such as src/limits.rs#Limits.resolve. Repeat it to show several symbols, joined \
by one blank line. A block that drifts fails code-block-stale, and `supdoc fix --yes` copies \
the current text in.
  uses(PATH#SYMBOL)
      A hand-written block that depends on the symbol, tracked in supdoc.lock like a link.
  fragment(KIND)
      The block is part of a larger construct: statements, expression, entries, children, \
members or declarations. A step whose tool can't check that kind doesn't run, with a \
code-block-fragment-unsupported warning. Some tools read a kind only inside a frame (below).
  ignore(reason)
      Check nothing in this block.
  format(off: reason), lint(off: reason)
      Skip only that step for this block.
  format(full), lint(full)
      Run that step even though its tool's table turns it off.

Attributes are always parenthesized, never key=value, and a comma doesn't separate them. An \
attribute a block's tools don't take warns code-block-unknown-attribute. An ignore or off without \
a reason still applies, with a code-block-missing-reason warning. GitHub, markdown-it, Docusaurus, \
mdBook and TypeDoc hide attributes; Mintlify shows the text after the language as the block's \
title.

supdoc checks what is written and never skips a block on its own. The exception is a Rust block \
that reads a file or an environment variable at compile time (include_str!, env!), or, before \
clippy compiles it, one that calls a macro off supdoc's list: it isn't checked, with a \
code-block-not-checked warning. In a trusted checkout, a Rust block that names an item it \
doesn't define fails code-block-missing-name, once per name.

A tool reads some fragment kinds only inside a frame: fixed lines before and after the block, \
the same for every block of that kind and never chosen from its content. No finding shows a \
frame line and a fix never writes one. A block the tool can't parse inside its frame fails \
code-block-parse \"couldn't parse the block as KIND\" and nothing else, never with the tool's \
own message. In a block that parsed, a finding on a frame line is reported at the block's \
attributes, and no rule is left off. A block that closes \
the frame itself (a lone } then more code) is linted as written; rustfmt's format step fails \
it code-block-tool-failed \"reformatted the fragment's frame\", with no fix. The frames, each \
block standing where the ... is:

  rustfmt       statements    fn supdoc_fragment() { ... }
  rustfmt       expression    fn supdoc_fragment() { ... }
  biome-lint    statements    async function supdocFragment$() { await Promise.resolve(); ... }
  biome-lint    expression    ( ... )
  biome-lint    entries       const supdocFragment$ = { ... };
  biome-lint    children      const supdocFragment$ = <div> ... </div>;   (tsx and jsx blocks only)
  biome-lint    members       abstract class SupdocFragment$ { supdocFrame$() { return this; } ... }   (TypeScript blocks)
  biome-lint    members       class SupdocFragment$ { supdocFrame$() { return this; } ... }   (JavaScript blocks)
  biome-format  statements    async function supdocFragment$() { await Promise.resolve(); ... }
  biome-format  entries       const supdocFragment$ = { ... };
  biome-format  children      const supdocFragment$ = <div> ... </div>;   (tsx and jsx blocks only)
  biome-format  members       abstract class SupdocFragment$ { supdocFrame$() { return this; } ... }   (TypeScript blocks)
  biome-format  members       class SupdocFragment$ { supdocFrame$() { return this; } ... }   (JavaScript blocks)

A statements block's leading \"use strict\" or \"use server\" isn't read as a directive, since \
the frame's first line stands before it. A members frame has no base class, so a constructor \
that calls super() isn't a member it can hold.

A project that turns on rules about the frame's own code, such as biome's opt-in nursery rules \
wanting an explicit type or a function expression, gets their findings at the block's \
attributes; switch the step off for such a block with lint(off: reason).

rustfmt formats a framed block with its max_width raised by one indent and every width it \
derives from max_width held to the project's, so it lays out the block as the project's own \
rustfmt would. A nightly rustfmt whose config sets brace_style, fn_single_line or \
blank_lines_lower_bound away from its default would reshape the frame, so those blocks' format \
step doesn't run, with a code-block-not-checked warning.

biome formats a framed block with its lineWidth raised by the indent the frame adds (two indents \
in a children block, which biome wraps in parentheses), so it wraps the block where the \
project's own biome would. A config whose overrides set indentStyle, indentWidth or lineWidth \
leaves those blocks' format step not run, with a code-block-not-checked warning.

The summary counts each step: a step its tool's table sets to \"off\" is `format off` or `lint \
off`; one a block marks off is `format skipped` or `lint skipped`; one that was due but didn't \
run, such as clippy in a checkout you don't trust, is `lint not run` with its reason, never a \
pass.

A Rust block that needs surrounding code takes one of three forms: a from(...) block of the real \
code; the context written into the block as ordinary code (use lines, fn main() { ... }); or the \
block kept partial and marked lint(off: needs crate context), or ignore(placeholder) when it \
doesn't parse on its own.";

/// `supdoc fix --help`: what fix repairs, and what it refuses.
pub(crate) const FIX_LONG_ABOUT: &str = "\
Repair broken links in docs and comments (writes; previews unless --yes)

Repairs a broken link only when its fix is certain, rewrites a stale from(...) code block from \
its source, and replaces an unformatted code block with its formatter's output.

A code-block fix is refused, with a code-block-fix-unsafe warning, when the new body could end \
the block early: it holds a control character other than a tab or a line ending, one of its \
lines would close the fence, or the doc, parsed again with the fix applied, no longer holds the \
same fenced blocks.";

/// `supdoc trust --help`: what trust grants, where it is stored, and where never to grant it.
pub(crate) const TRUST_LONG_ABOUT: &str = "\
Trust a checkout to run its code blocks' tools, or revoke it (writes your state directory)

In any checkout, code-block checks run only tools that read the block and plain config files. \
Trusting a checkout also lets clippy lint its Rust blocks against the project's crate: supdoc \
runs `cargo check --lib --locked --offline` on the root's Cargo.toml, so the project's build \
scripts and proc macros run. That is what trust grants. cargo reads your cargo config as it does \
when you run it in the root, and builds the path dependencies, workspace members and [patch] \
paths the root names. supdoc never writes Cargo.lock and never downloads a crate: run `cargo \
generate-lockfile` or `cargo fetch` first. A root package that would join a workspace outside the root \
isn't built. cargo and clippy get a cleared environment, so \
secrets in your environment don't reach them; files your user can read still can.

The build goes into ~/.cache/supdoc/code-blocks/target/<hash of the root> (under \
$XDG_CACHE_HOME when it's set), never inside the checkout. Each trusted checkout, and each of \
its worktrees, gets a full build there, which can take as much disk as the project's own. Delete \
the directory to get the space back.

Trust is granted only from outside the checkout. `supdoc trust` records the project root in \
$XDG_STATE_HOME/supdoc/trusted-roots (else ~/.local/state/supdoc/trusted-roots); \
SUPDOC_TRUST=1 trusts one run, as in a CI job, and SUPDOC_TRUST=0 turns trust off for one run \
whatever the store lists. No file in the checkout can turn trust on. `tier = \"static\"` in \
[features.code-blocks] holds every step to the untrusted list even in a trusted checkout, with no \
warning. supdoc doesn't check who owns a trusted directory or the store: anyone who can write \
the store can grant trust. `supdoc trust` refuses $HOME, its ancestors and /.

Trust follows the directory, not its contents. A trusted checkout switched to a branch you don't \
trust builds that branch, and a linked worktree of a trusted repository at the same path inside \
its work tree is trusted whatever it has checked out: review an untrusted pull request there \
with SUPDOC_TRUST=0. Remove worktrees with `git worktree remove`, or run `git worktree prune` \
after deleting one by hand, or a later directory at that path can inherit trust.

In CI, never set SUPDOC_TRUST in a pull_request_target or workflow_run workflow, or in any job \
that has secrets and checks out a pull request's code. Set it per job: a repository- or \
organization-level variable reaches every workflow. On a persistent or self-hosted runner, never \
run `supdoc trust` on a checkout path the runner reuses, and set SUPDOC_TRUST=0 in pull request \
jobs.";

/// `supdoc schema --help`: what a consumer of `--json` output may rely on.
pub(crate) const SCHEMA_LONG_ABOUT: &str = "\
Print the JSON Schema of a command's --json output (read-only)

Every --json result carries schema_version. An error document, the error entry of `supdoc \
schema`, doesn't. Adding an optional field, or a new value of an \
existing enum (states, actions, kinds, JSONL record types), keeps it; removing or renaming \
anything bumps it. A consumer must tolerate values it doesn't know.";

/// `supdoc skill install --help`: where the copies go, and what is refused.
pub(crate) const SKILL_INSTALL_LONG_ABOUT: &str = "\
Install SKILL.md for agent runtimes (writes; previews unless --yes)

Writes two copies of the SKILL.md embedded in this binary: .agents/skills/supdoc/SKILL.md for \
agents that read the shared directory, and .claude/skills/supdoc/SKILL.md for Claude Code. With \
--global they go under $HOME, and Claude Code's copy under $CLAUDE_CONFIG_DIR when it's set. An \
existing SKILL.md with different contents is replaced only with --yes. A project install writes \
nothing outside the project: a symlinked .agents or .claude that leads out of it is named and \
refused.";

/// `supdoc upgrade --help`: which installs it handles, and its network access.
pub(crate) const UPGRADE_LONG_ABOUT: &str = "\
Install a newer or older supdoc with cargo (writes; previews unless --yes; to accept code \
changes, see `update`)

Works only when this binary was installed with `cargo install` from crates.io; any other install \
is refused, naming what supdoc sees and the manual step. This is supdoc's only network access: \
it runs cargo, which talks to crates.io. No command checks for updates on its own.";
