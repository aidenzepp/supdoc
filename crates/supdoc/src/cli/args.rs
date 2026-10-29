//! Command-line argument definitions, written with clap's derive API (06).
//!
//! Every flag has a `SUPDOC_*` environment variable (06 X6). Definition only: running a command
//! lives in the sibling modules.

use std::path::PathBuf;

use clap::builder::FalseyValueParser;
use clap::{ArgAction, Args, ColorChoice, Parser, Subcommand, ValueEnum, ValueHint};

use super::examples::{
    CHECK_EXAMPLES, COMPLETIONS_EXAMPLES, CONFIG_EXAMPLES, CONFIG_SHOW_EXAMPLES, FIX_EXAMPLES,
    INIT_EXAMPLES, LIST_EXAMPLES, NAMESPACE_EXAMPLES, NAMESPACE_LIST_EXAMPLES,
    NAMESPACE_RENAME_EXAMPLES, RESOLVE_EXAMPLES, SCHEMA_EXAMPLES, SKILL_EXAMPLES,
    SKILL_INSTALL_EXAMPLES, TOP_EXAMPLES, UPDATE_EXAMPLES,
};
use crate::link::{LinkRef, SymbolPath};
use crate::namespace::NamespaceName;
use crate::output::OutputFormat;
use crate::state::LinkState;

/// Keep documentation in step with the code it describes.
///
/// supdoc tracks Markdown links from docs and code comments to code, and reports when the code
/// they point at changes.
///
/// A link names a file, or a symbol inside it, such as `[add](src/lib.rs#add)`. `supdoc.lock`
/// records a hash of each linked target. `supdoc check` compares today's code with those hashes
/// and fails when a documented target changed, so someone rereads the docs. `supdoc update`
/// records the new hashes once the docs are confirmed accurate.
///
/// Reads never write. `update`, `fix`, `namespace rename` and `skill install` preview unless given `--yes`; `init` writes
/// immediately (`--dry-run` previews it). With `--json` every
/// result, and every error, is a JSON document. Exit codes: 0 clean, 1 failing or refused,
/// 2 usage error, 70 supdoc bug, 74 I/O error, 78 config or lockfile error.
#[derive(Debug, Parser)]
#[command(
    name = "supdoc",
    version,
    propagate_version = true,
    arg_required_else_help = true,
    after_help = TOP_EXAMPLES
)]
pub struct Cli {
    /// Flags every command accepts.
    #[command(flatten)]
    pub global: GlobalArgs,
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Flags accepted by every command, before or after the subcommand.
#[derive(Debug, Clone, Args)]
#[command(next_help_heading = "Global")]
pub struct GlobalArgs {
    /// Use this config file instead of searching upward for supdoc.config.toml
    #[arg(
        short,
        long,
        global = true,
        env = "SUPDOC_CONFIG",
        value_name = "PATH",
        value_hint = ValueHint::FilePath
    )]
    pub config: Option<PathBuf>,
    /// Output format
    #[arg(
        long,
        global = true,
        env = "SUPDOC_OUTPUT",
        value_enum,
        default_value_t = OutputFormat::Text,
        value_name = "FORMAT"
    )]
    pub output: OutputFormat,
    /// Same as --output json
    #[arg(long, global = true, env = "SUPDOC_JSON", value_parser = FalseyValueParser::new())]
    pub json: bool,
    /// Same as --output jsonl
    #[arg(long, global = true, env = "SUPDOC_JSONL", value_parser = FalseyValueParser::new())]
    pub jsonl: bool,
    /// Only print data and errors
    #[arg(short, long, global = true, env = "SUPDOC_QUIET", value_parser = FalseyValueParser::new())]
    pub quiet: bool,
    /// Print more diagnostic detail (repeat for more)
    #[arg(short, long, global = true, env = "SUPDOC_VERBOSE", action = ArgAction::Count)]
    pub verbose: u8,
    /// When to use color
    #[arg(
        long,
        global = true,
        env = "SUPDOC_COLOR",
        value_enum,
        default_value_t = ColorChoice::Auto,
        value_name = "WHEN"
    )]
    pub color: ColorChoice,
}

impl GlobalArgs {
    /// The format that applies once `--json` and `--jsonl` are taken into account.
    ///
    /// `--jsonl` beats `--json`, which beats `--output` (06 X5). This is precedence rather than a
    /// clap conflict, because a conflict could fire from `SUPDOC_OUTPUT` alone.
    #[must_use]
    pub fn format(&self) -> OutputFormat {
        resolve_format(self.output, self.json, self.jsonl)
    }
}

/// Applies the precedence `--jsonl` > `--json` > `--output` (06 X5).
#[must_use]
pub(super) const fn resolve_format(output: OutputFormat, json: bool, jsonl: bool) -> OutputFormat {
    if jsonl {
        OutputFormat::Jsonl
    } else if json {
        OutputFormat::Json
    } else {
        output
    }
}

/// The commands (06 X2). Actions are top-level verbs, groups are `noun verb`.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check links against supdoc.lock (read-only)
    #[command(after_help = CHECK_EXAMPLES)]
    Check(CheckArgs),
    /// Accept changes into supdoc.lock (writes; previews unless --yes)
    #[command(after_help = UPDATE_EXAMPLES)]
    Update(UpdateArgs),
    /// Repair broken links in docs and comments (writes; previews unless --yes)
    #[command(after_help = FIX_EXAMPLES)]
    Fix(FixArgs),
    /// List links and their states (read-only)
    #[command(after_help = LIST_EXAMPLES)]
    List(ListArgs),
    /// List the symbols a file exposes, or check that one resolves (read-only)
    #[command(after_help = RESOLVE_EXAMPLES)]
    Resolve(ResolveArgs),
    /// Write a starter supdoc.config.toml (writes; refuses to overwrite)
    #[command(after_help = INIT_EXAMPLES)]
    Init(InitArgs),
    /// Inspect configuration
    #[command(subcommand, after_help = CONFIG_EXAMPLES)]
    Config(ConfigCommand),
    /// Inspect or rename namespaces
    #[command(subcommand, after_help = NAMESPACE_EXAMPLES)]
    Namespace(NamespaceCommand),
    /// Print the JSON Schema of a command's --json output (read-only)
    #[command(after_help = SCHEMA_EXAMPLES)]
    Schema(SchemaArgs),
    /// Print a shell completion script (read-only)
    #[command(after_help = COMPLETIONS_EXAMPLES)]
    Completions(CompletionsArgs),
    /// Install supdoc's agent skill
    #[command(subcommand, after_help = SKILL_EXAMPLES)]
    Skill(SkillCommand),
}

/// Whether a write command applies its changes (06 X4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// Show what would change and write nothing.
    Preview,
    /// Make the changes.
    Apply,
}

/// Flags shared by every command that writes (06 X4).
#[derive(Debug, Clone, Args)]
#[command(next_help_heading = "Writing")]
pub struct Confirm {
    /// Show what would change and write nothing
    #[arg(long, env = "SUPDOC_DRY_RUN", value_parser = FalseyValueParser::new())]
    pub dry_run: bool,
    /// Apply the changes (without this, the command previews)
    #[arg(short, long, env = "SUPDOC_YES", value_parser = FalseyValueParser::new())]
    pub yes: bool,
}

impl Confirm {
    /// `--yes` applies, `--dry-run` or neither previews, and `--dry-run` beats `--yes`.
    #[must_use]
    pub const fn mode(&self) -> WriteMode {
        if self.yes && !self.dry_run {
            WriteMode::Apply
        } else {
            WriteMode::Preview
        }
    }
}

/// Arguments of `supdoc check`.
#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Only links written in these files or directories [default: the whole project]
    #[arg(value_name = "PATH", value_hint = ValueHint::AnyPath)]
    pub paths: Vec<PathBuf>,
    /// Obsolete entries and syntax-error targets also fail
    #[arg(long, env = "SUPDOC_DENY_WARNINGS", value_parser = FalseyValueParser::new())]
    pub deny_warnings: bool,
}

/// Arguments of `supdoc update`.
#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Only links written in these files or directories [default: the whole project]
    #[arg(value_name = "PATH", value_hint = ValueHint::AnyPath)]
    pub paths: Vec<PathBuf>,
    /// Which states to accept
    #[arg(
        long,
        env = "SUPDOC_STATE",
        value_enum,
        value_delimiter = ',',
        value_name = "STATE",
        default_values_t = [LinkState::Failed, LinkState::New, LinkState::Obsolete]
    )]
    pub state: Vec<LinkState>,
    /// Preview or apply.
    #[command(flatten)]
    pub confirm: Confirm,
}

/// Arguments of `supdoc fix`.
#[derive(Debug, Args)]
pub struct FixArgs {
    /// Only links written in these files or directories [default: the whole project]
    #[arg(value_name = "PATH", value_hint = ValueHint::AnyPath)]
    pub paths: Vec<PathBuf>,
    /// Preview or apply.
    #[command(flatten)]
    pub confirm: Confirm,
}

/// Arguments of `supdoc list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Only links to this target: `path`, `path#symbol` or `ns:path#symbol`
    #[arg(value_name = "TARGET")]
    pub target: Option<LinkRef>,
    /// Only links in these states
    #[arg(
        long,
        env = "SUPDOC_STATE",
        value_enum,
        value_delimiter = ',',
        value_name = "STATE"
    )]
    pub state: Vec<LinkState>,
    /// Show at most this many links; 0 means no limit
    #[arg(long, env = "SUPDOC_LIMIT", default_value_t = DEFAULT_LIMIT, value_name = "N")]
    pub limit: usize,
}

/// How many results `list` and `resolve` show unless told otherwise (06 X11).
pub const DEFAULT_LIMIT: usize = 100;

/// Arguments of `supdoc resolve`.
#[derive(Debug, Args)]
pub struct ResolveArgs {
    /// The file whose symbols to list
    #[arg(value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub file: PathBuf,
    /// Check that this symbol resolves in FILE, such as `Config.validate`
    #[arg(value_name = "SYMBOL")]
    pub symbol: Option<SymbolPath>,
    /// Show at most this many symbols; 0 means no limit
    #[arg(long, env = "SUPDOC_LIMIT", default_value_t = DEFAULT_LIMIT, value_name = "N")]
    pub limit: usize,
}

/// Arguments of `supdoc init`.
#[derive(Debug, Args)]
#[command(next_help_heading = "Writing")]
pub struct InitArgs {
    /// Show what would be written and write nothing
    #[arg(long, env = "SUPDOC_DRY_RUN", value_parser = FalseyValueParser::new())]
    pub dry_run: bool,
}

/// `supdoc config ...`.
#[derive(Debug, Subcommand)]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub enum ConfigCommand {
    /// Print the resolved config and which file each value came from (read-only)
    #[command(after_help = CONFIG_SHOW_EXAMPLES)]
    Show,
}

/// `supdoc namespace ...`.
#[derive(Debug, Subcommand)]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub enum NamespaceCommand {
    /// List namespaces and the directories they map to (read-only)
    #[command(after_help = NAMESPACE_LIST_EXAMPLES)]
    List,
    /// Rename a namespace in config, supdoc.lock and every link using it (writes; previews unless --yes)
    #[command(after_help = NAMESPACE_RENAME_EXAMPLES)]
    Rename(RenameArgs),
}

/// Arguments of `supdoc namespace rename`.
#[derive(Debug, Args)]
pub struct RenameArgs {
    /// The namespace to rename
    #[arg(value_name = "OLD")]
    pub old: NamespaceName,
    /// The new name
    #[arg(value_name = "NEW")]
    pub new: NamespaceName,
    /// Preview or apply.
    #[command(flatten)]
    pub confirm: Confirm,
}

/// What `config show` and `namespace list` read, handed to the one module that implements both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inspect {
    /// `supdoc config show`.
    ConfigShow,
    /// `supdoc namespace list`.
    NamespaceList,
}

/// Arguments of `supdoc schema`.
#[derive(Debug, Args)]
pub struct SchemaArgs {
    /// The command whose `--json` output to describe [default: every command]
    #[arg(value_enum, value_name = "COMMAND")]
    pub command: Option<SchemaTarget>,
}

/// The commands that have JSON output, and so a schema (06 X10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SchemaTarget {
    /// `supdoc check`
    Check,
    /// `supdoc update`
    Update,
    /// `supdoc fix`
    Fix,
    /// `supdoc list`
    List,
    /// `supdoc resolve`
    Resolve,
    /// `supdoc config show`
    ConfigShow,
    /// `supdoc namespace list`
    NamespaceList,
    /// `supdoc namespace rename`
    NamespaceRename,
    /// `supdoc init`
    Init,
    /// `supdoc skill install`
    SkillInstall,
}

/// Arguments of `supdoc completions`.
#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// The shell to generate completions for
    #[arg(value_enum, value_name = "SHELL")]
    pub shell: clap_complete::Shell,
}

/// `supdoc skill ...`.
#[derive(Debug, Subcommand)]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub enum SkillCommand {
    /// Install SKILL.md for agent runtimes (writes; previews unless --yes)
    #[command(after_help = SKILL_INSTALL_EXAMPLES)]
    Install(SkillInstallArgs),
}

/// Arguments of `supdoc skill install`.
#[derive(Debug, Args)]
pub struct SkillInstallArgs {
    /// Install into the user's home instead of the current project
    #[arg(short, long, env = "SUPDOC_GLOBAL", value_parser = FalseyValueParser::new())]
    pub global: bool,
    /// Preview or apply.
    #[command(flatten)]
    pub confirm: Confirm,
}
