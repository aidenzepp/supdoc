//! The long help each command shows under `--help`, above its flags.
//!
//! `-h` shows the one-line summary from the command's doc comment; `--help` shows these texts.
//! Each one starts with that same summary line, so the two never disagree about what the command
//! is. These texts are the reference for behavior a user sees: the README points here.

/// `supdoc check --help`: the gate, and how it checks code blocks.
pub(crate) const CHECK_LONG_ABOUT: &str = "\
Check links against supdoc.lock (read-only)

Exits 1 when a link needs attention: its target changed since the lock recorded it, it no longer \
resolves, or it has no lock entry yet. With --deny-warnings, warnings such as obsolete lock \
entries also fail.

Code blocks are checked only when the config has a [features.code-blocks] table; an empty table \
turns it on. A language's table, such as [features.code-blocks.shell], turns that language on, \
and each tool's options live in the tool's own table, such as [features.code-blocks.python.ruff]. \
Write attributes after a block's language, separated by spaces:

  from(PATH#SYMBOL)
      The block is exactly that symbol's text. It fails code-block-stale when it drifts, and \
`supdoc fix --yes` copies the current text in. Repeat it to join several symbols.
  uses(PATH#SYMBOL)
      A hand-written block that depends on the symbol, tracked in supdoc.lock like a link.
  ignore(reason)
      Check nothing in this block.
  format(off: reason), lint(off: reason)
      Skip only that step for this block.
  format(full), lint(full)
      Run that step even though its tool's table turns it off.

Attributes are always parenthesized, never key=value, and a comma doesn't separate them. An \
ignore or off without a reason still applies, with a code-block-missing-reason warning.

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
generate-lockfile` or `cargo fetch` first. cargo and clippy get a cleared environment, so \
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

Every --json document carries schema_version. Adding an optional field, or a new value of an \
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
