//! Example invocations shown after each command's flags in `--help` (06 X9).
//!
//! Every example is a real invocation with realistic arguments. A test parses each one, so an
//! example can never drift away from the flags it demonstrates. Group blocks are built from the
//! leaf blocks' bodies, so the two can never differ.

/// Shown after the top-level help: the three tasks people reach for first.
pub(crate) const TOP_EXAMPLES: &str = "\
Examples:
  # Are the docs still accurate? (CI gate)
  supdoc check
  supdoc check --json

  # I reread docs/guide.md after the code changed and it's still right
  supdoc update docs/guide.md --yes

  # Who documents this function before I change it?
  supdoc list src/lib.rs#add";

/// Shown after `supdoc check --help`.
pub(crate) const CHECK_EXAMPLES: &str = "\
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
pub(crate) const UPDATE_EXAMPLES: &str = "\
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
pub(crate) const FIX_EXAMPLES: &str = "\
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
pub(crate) const LIST_EXAMPLES: &str = "\
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
pub(crate) const RESOLVE_EXAMPLES: &str = "\
Examples:
  # The symbols a file exposes
  supdoc resolve src/config.rs

  # Does this symbol resolve?
  supdoc resolve src/config.rs Config.validate

  # A Markdown file's headings, machine-readable
  supdoc resolve docs/guide.md --json";

/// Body of the `config show` examples, shared by the leaf and the `config` group blocks.
macro_rules! config_show_body {
    () => {
        "
  # The resolved config and the file each value came from
  supdoc config show

  # Machine-readable
  supdoc config show --json"
    };
}

/// Body of the `namespace list` examples, shared by the leaf and the `namespace` group blocks.
macro_rules! namespace_list_body {
    () => {
        "
  # Namespaces and the directories they map to
  supdoc namespace list

  # Machine-readable
  supdoc namespace list --json"
    };
}

/// Body of the `namespace rename` examples, shared by the leaf and the `namespace` group blocks.
macro_rules! namespace_rename_body {
    () => {
        "
  # Preview renaming `auth` to `identity` everywhere
  supdoc namespace rename auth identity

  # Apply it
  supdoc namespace rename auth identity --yes

  # Machine-readable preview
  supdoc namespace rename auth identity --json"
    };
}

/// Body of the `skill install` examples, shared by the leaf and the `skill` group blocks.
macro_rules! skill_install_body {
    () => {
        "
  # Preview the install into the current project
  supdoc skill install

  # Install into your home directory
  supdoc skill install --global --yes

  # Machine-readable preview
  supdoc skill install --json"
    };
}

/// Shown after `supdoc config show --help`.
pub(crate) const CONFIG_SHOW_EXAMPLES: &str = concat!("Examples:", config_show_body!());

/// Shown after `supdoc namespace list --help`.
pub(crate) const NAMESPACE_LIST_EXAMPLES: &str = concat!("Examples:", namespace_list_body!());

/// Shown after `supdoc namespace rename --help`.
pub(crate) const NAMESPACE_RENAME_EXAMPLES: &str = concat!("Examples:", namespace_rename_body!());

/// Shown after `supdoc init --help`.
pub(crate) const INIT_EXAMPLES: &str = "\
Examples:
  # Write a starter supdoc.config.toml
  supdoc init

  # Show what would be written
  supdoc init --dry-run

  # Machine-readable preview
  supdoc init --dry-run --json";

/// Shown after `supdoc schema --help`.
pub(crate) const SCHEMA_EXAMPLES: &str = "\
Examples:
  # The JSON Schema of `supdoc check --json`
  supdoc schema check

  # Every command's schema
  supdoc schema";

/// Shown after `supdoc completions --help`.
pub(crate) const COMPLETIONS_EXAMPLES: &str = "\
Examples:
  # Install zsh completions
  supdoc completions zsh > ~/.zfunc/_supdoc

  # Machine-readable
  supdoc completions zsh --json";

/// Shown after `supdoc skill install --help`.
pub(crate) const SKILL_INSTALL_EXAMPLES: &str = concat!("Examples:", skill_install_body!());

/// Shown after `supdoc config --help`.
pub(crate) const CONFIG_EXAMPLES: &str = concat!("Examples:", config_show_body!());

/// Shown after `supdoc namespace --help`.
pub(crate) const NAMESPACE_EXAMPLES: &str = concat!(
    "Examples:",
    namespace_list_body!(),
    "\n",
    namespace_rename_body!()
);

/// Shown after `supdoc skill --help`.
pub(crate) const SKILL_EXAMPLES: &str = concat!("Examples:", skill_install_body!());
