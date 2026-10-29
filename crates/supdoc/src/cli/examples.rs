//! Example invocations shown after each command's flags in `--help` (06 X9).
//!
//! Every example is a real invocation with realistic arguments. A test parses each one, so an
//! example can never drift away from the flags it demonstrates.

/// Shown after the top-level help: the three tasks people reach for first.
pub const TOP_EXAMPLES: &str = "\
Examples:
  # Are the docs still accurate? (CI gate)
  supdoc check
  supdoc check --json

  # I reread docs/guide.md after the code changed and it's still right
  supdoc update docs/guide.md --yes

  # Who documents this function before I change it?
  supdoc list src/lib.rs#add";

/// Shown after `supdoc check --help`.
pub const CHECK_EXAMPLES: &str = "\
Examples:
  # Check every link in the project
  supdoc check

  # Check only the links written under docs/
  supdoc check docs/

  # Annotate a pull request in GitHub Actions
  supdoc check --output github

  # Machine-readable result
  supdoc check --json";

/// Shown after `supdoc update --help`.
pub const UPDATE_EXAMPLES: &str = "\
Examples:
  # Preview what would be accepted into supdoc.lock
  supdoc update

  # Record new and obsolete entries only (bookkeeping, no content changes)
  supdoc update --state new,obsolete --yes

  # I reread docs/guide.md and it is still right
  supdoc update docs/guide.md --yes

  # Machine-readable preview
  supdoc update --dry-run --json";

/// Shown after `supdoc fix --help`.
pub const FIX_EXAMPLES: &str = "\
Examples:
  # Preview the repairs
  supdoc fix

  # Apply every repair
  supdoc fix --yes

  # Repair only the links in docs/api.md
  supdoc fix docs/api.md --yes

  # Machine-readable preview
  supdoc fix --dry-run --json";

/// Shown after `supdoc list --help`.
pub const LIST_EXAMPLES: &str = "\
Examples:
  # Every link and its state (first 100)
  supdoc list

  # Who documents this function?
  supdoc list src/lib.rs#add

  # Only the links whose code changed
  supdoc list --state failed

  # A namespaced target, machine-readable
  supdoc list auth:src/session.rs --json";

/// Shown after `supdoc resolve --help`.
pub const RESOLVE_EXAMPLES: &str = "\
Examples:
  # The symbols a file exposes
  supdoc resolve src/config.rs

  # Does this symbol resolve?
  supdoc resolve src/config.rs Config.validate

  # A Markdown file's headings, machine-readable
  supdoc resolve docs/guide.md --json";

/// Shown after `supdoc config show --help`.
pub const CONFIG_SHOW_EXAMPLES: &str = "\
Examples:
  # The resolved config and the file each value came from
  supdoc config show

  # Machine-readable
  supdoc config show --json";

/// Shown after `supdoc namespace list --help`.
pub const NAMESPACE_LIST_EXAMPLES: &str = "\
Examples:
  # Namespaces and the directories they map to
  supdoc namespace list

  # Machine-readable
  supdoc namespace list --json";

/// Shown after `supdoc namespace rename --help`.
pub const NAMESPACE_RENAME_EXAMPLES: &str = "\
Examples:
  # Preview renaming `auth` to `identity` everywhere
  supdoc namespace rename auth identity

  # Apply it
  supdoc namespace rename auth identity --yes";

/// Shown after `supdoc init --help`.
pub const INIT_EXAMPLES: &str = "\
Examples:
  # Write a starter supdoc.config.toml
  supdoc init

  # Show what would be written
  supdoc init --dry-run";

/// Shown after `supdoc schema --help`.
pub const SCHEMA_EXAMPLES: &str = "\
Examples:
  # The JSON Schema of `supdoc check --json`
  supdoc schema check

  # Every command's schema
  supdoc schema";

/// Shown after `supdoc completions --help`.
pub const COMPLETIONS_EXAMPLES: &str = "\
Examples:
  # Install zsh completions
  supdoc completions zsh > ~/.zfunc/_supdoc";

/// Shown after `supdoc skill install --help`.
pub const SKILL_INSTALL_EXAMPLES: &str = "\
Examples:
  # Preview the install into the current project
  supdoc skill install

  # Install into your home directory
  supdoc skill install --global --yes";
