//! Command-line argument definitions, written with clap's derive API.
//!
//! Every flag has a `SUPDOC_*` environment variable. Definition only: running a command
//! lives in the sibling modules.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::str::FromStr as _;

use clap::builder::FalseyValueParser;
use clap::{ArgAction, Args, ColorChoice, Parser, Subcommand, ValueEnum, ValueHint};

use super::examples::{
    CHECK_EXAMPLES, COMPLETIONS_EXAMPLES, CONFIG_EXAMPLES, CONFIG_SHOW_EXAMPLES, FIX_EXAMPLES,
    INIT_EXAMPLES, LIST_EXAMPLES, NAMESPACE_EXAMPLES, NAMESPACE_LIST_EXAMPLES,
    NAMESPACE_RENAME_EXAMPLES, RESOLVE_EXAMPLES, SCHEMA_EXAMPLES, SKILL_EXAMPLES,
    SKILL_INSTALL_EXAMPLES, TOP_EXAMPLES, UPDATE_EXAMPLES,
};
use crate::limits::{CommandLineLimits, Given, Limits, SizeLimit, TimeLimit};
use crate::link::SymbolPath;
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
pub(crate) struct Cli {
    /// Flags every command accepts.
    #[command(flatten)]
    pub(crate) global: GlobalArgs,
    /// What to do.
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Flags accepted by every command, before or after the subcommand.
#[derive(Debug, Clone, Args)]
#[command(next_help_heading = "Global")]
pub(crate) struct GlobalArgs {
    /// Use this config file instead of searching upward for supdoc.config.toml
    #[arg(
        short,
        long,
        global = true,
        env = "SUPDOC_CONFIG",
        value_name = "PATH",
        value_hint = ValueHint::FilePath
    )]
    pub(crate) config: Option<PathBuf>,
    /// Output format
    #[arg(
        long,
        global = true,
        env = "SUPDOC_OUTPUT",
        value_enum,
        default_value_t = OutputFormat::Text,
        value_name = "FORMAT"
    )]
    pub(crate) output: OutputFormat,
    /// Same as --output json
    #[arg(long, global = true, env = "SUPDOC_JSON", value_parser = FalseyValueParser::new())]
    pub(crate) json: bool,
    /// Same as --output jsonl
    #[arg(long, global = true, env = "SUPDOC_JSONL", value_parser = FalseyValueParser::new())]
    pub(crate) jsonl: bool,
    /// Only print data and errors
    #[arg(short, long, global = true, env = "SUPDOC_QUIET", value_parser = FalseyValueParser::new())]
    pub(crate) quiet: bool,
    /// Show more detail, such as passed links in `check`
    #[arg(short, long, global = true, env = "SUPDOC_VERBOSE", action = ArgAction::Count)]
    pub(crate) verbose: u8,
    /// When to use color
    #[arg(
        long,
        global = true,
        env = "SUPDOC_COLOR",
        value_enum,
        default_value_t = ColorChoice::Auto,
        value_name = "WHEN"
    )]
    pub(crate) color: ColorChoice,
    /// The `[limits]` ceilings given by flag or environment variable.
    #[command(flatten)]
    pub(crate) limits: LimitFlags,
}

/// The `--parse-timeout`, `--max-lock-size`, `--max-config-size` and `--max-ignore-size` flags.
///
/// Written by hand rather than derived, so each value keeps whether its flag or its `SUPDOC_*`
/// variable gave it, which `config show` reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LimitFlags(pub CommandLineLimits);

/// The clap id, flag, variable, value name, help and default of each limit flag.
const LIMIT_FLAGS: [(&str, &str, &str, &str); 4] = [
    (
        "parse-timeout",
        "SUPDOC_PARSE_TIMEOUT",
        "DURATION",
        "Stop each read of a file after this long, such as 10s or 500ms; 0 for no limit",
    ),
    (
        "max-lock-size",
        "SUPDOC_MAX_LOCK_SIZE",
        "SIZE",
        "Refuse a supdoc.lock larger than this, such as 16MiB; 0 for no limit",
    ),
    (
        "max-config-size",
        "SUPDOC_MAX_CONFIG_SIZE",
        "SIZE",
        "Refuse a config file larger than this, such as 256KiB; 0 for no limit",
    ),
    (
        "max-ignore-size",
        "SUPDOC_MAX_IGNORE_SIZE",
        "SIZE",
        "Refuse an ignore file larger than this, such as 256KiB; 0 for no limit",
    ),
];

impl LimitFlags {
    /// The default each flag's help shows, in [`LIMIT_FLAGS`] order.
    fn defaults() -> [String; 4] {
        [
            Limits::PARSE_TIMEOUT.to_string(),
            Limits::MAX_LOCK_SIZE.to_string(),
            Limits::MAX_CONFIG_SIZE.to_string(),
            Limits::MAX_IGNORE_SIZE.to_string(),
        ]
    }
}

/// The value of the limit flag `id`, and whether its environment variable gave it.
fn given<T: Copy + Send + Sync + 'static>(
    matches: &clap::ArgMatches,
    id: &str,
) -> Option<Given<T>> {
    let value = *matches.get_one::<T>(id)?;
    let from_env = matches.value_source(id) == Some(clap::parser::ValueSource::EnvVariable);
    Some(Given { value, from_env })
}

impl clap::FromArgMatches for LimitFlags {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        Ok(Self(CommandLineLimits {
            parse_timeout: given(matches, "parse-timeout"),
            max_lock_size: given(matches, "max-lock-size"),
            max_config_size: given(matches, "max-config-size"),
            max_ignore_size: given(matches, "max-ignore-size"),
        }))
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

impl Args for LimitFlags {
    fn augment_args(command: clap::Command) -> clap::Command {
        LIMIT_FLAGS.into_iter().zip(Self::defaults()).fold(
            command,
            |command, ((id, env, value_name, help), default)| {
                let arg = clap::Arg::new(id)
                    .long(id)
                    .global(true)
                    .env(env)
                    .value_name(value_name)
                    .help(format!("{help} [default: {default}]"))
                    .help_heading("Limits");
                command.arg(if value_name == "DURATION" {
                    arg.value_parser(TimeLimit::from_str)
                } else {
                    arg.value_parser(SizeLimit::from_str)
                })
            },
        )
    }

    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        Self::augment_args(command)
    }
}

impl GlobalArgs {
    /// The format that applies once `--json` and `--jsonl` are taken into account.
    ///
    /// `--jsonl` beats `--json`, which beats `--output`. This is precedence rather than a
    /// clap conflict, because a conflict could fire from `SUPDOC_OUTPUT` alone.
    #[must_use]
    pub(crate) fn format(&self) -> OutputFormat {
        resolve_format(self.output, self.json, self.jsonl)
    }
}

/// Applies the precedence `--jsonl` > `--json` > `--output`.
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

/// The commands. Actions are top-level verbs, groups are `noun verb`.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
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

/// Whether a write command applies its changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteMode {
    /// Show what would change and write nothing.
    Preview,
    /// Make the changes.
    Apply,
}

/// Flags shared by every command that writes.
#[derive(Debug, Clone, Args)]
#[command(next_help_heading = "Writing")]
pub(crate) struct Confirm {
    /// Show what would change and write nothing
    #[arg(long, env = "SUPDOC_DRY_RUN", value_parser = FalseyValueParser::new())]
    pub(crate) dry_run: bool,
    /// Apply the changes (without this, the command previews)
    #[arg(short, long, env = "SUPDOC_YES", value_parser = FalseyValueParser::new())]
    pub(crate) yes: bool,
}

impl Confirm {
    /// `--yes` applies, `--dry-run` or neither previews, and `--dry-run` beats `--yes`.
    #[must_use]
    pub(crate) const fn mode(&self) -> WriteMode {
        if self.yes && !self.dry_run {
            WriteMode::Apply
        } else {
            WriteMode::Preview
        }
    }
}

/// Arguments of `supdoc check`.
#[derive(Debug, Args)]
pub(crate) struct CheckArgs {
    /// Only links written in these files or directories [default: the whole project]
    #[arg(value_name = "PATH", value_hint = ValueHint::AnyPath)]
    pub(crate) paths: Vec<PathBuf>,
    /// Obsolete entries and syntax-error targets also fail
    #[arg(long, env = "SUPDOC_DENY_WARNINGS", value_parser = FalseyValueParser::new())]
    pub(crate) deny_warnings: bool,
}

/// Arguments of `supdoc update`.
#[derive(Debug, Args)]
pub(crate) struct UpdateArgs {
    /// Only links written in these files or directories [default: the whole project]
    #[arg(value_name = "PATH", value_hint = ValueHint::AnyPath)]
    pub(crate) paths: Vec<PathBuf>,
    /// Which states to accept
    #[arg(
        long,
        env = "SUPDOC_STATE",
        value_enum,
        value_delimiter = ',',
        value_name = "STATE",
        default_values_t = [LinkState::Failed, LinkState::New, LinkState::Obsolete]
    )]
    pub(crate) state: Vec<LinkState>,
    /// Preview or apply.
    #[command(flatten)]
    pub(crate) confirm: Confirm,
}

/// Arguments of `supdoc fix`.
#[derive(Debug, Args)]
pub(crate) struct FixArgs {
    /// Only links written in these files or directories [default: the whole project]
    #[arg(value_name = "PATH", value_hint = ValueHint::AnyPath)]
    pub(crate) paths: Vec<PathBuf>,
    /// Preview or apply.
    #[command(flatten)]
    pub(crate) confirm: Confirm,
}

/// Arguments of `supdoc list`.
#[derive(Debug, Args)]
pub(crate) struct ListArgs {
    /// Only links to this target: `path`, `path#symbol` or `ns:path#symbol`
    #[arg(value_name = "TARGET", value_hint = ValueHint::AnyPath)]
    pub(crate) target: Option<TargetArg>,
    /// Only links in these states
    #[arg(
        long,
        env = "SUPDOC_STATE",
        value_enum,
        value_delimiter = ',',
        value_name = "STATE"
    )]
    pub(crate) state: Vec<LinkState>,
    /// Show at most this many links; 0 means no limit
    #[arg(long, env = "SUPDOC_LIMIT", default_value_t = DEFAULT_LIMIT, value_name = "N")]
    pub(crate) limit: Limit,
}

/// A `list` TARGET as typed, split once into its file part and optional symbol.
///
/// Whether the file part is a `ns:path` is decided here, once, so nothing downstream re-splits
/// the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetArg {
    /// The text exactly as typed.
    typed: String,
    /// The text before the first `#`.
    file: String,
    /// The text after the first `#`, when there is one.
    symbol: Option<String>,
    /// Whether `file` is `ns:path`, with a valid namespace name and a non-empty path.
    namespaced: bool,
}

impl TargetArg {
    /// The target exactly as typed, for messages.
    #[must_use]
    pub(crate) fn typed(&self) -> &str {
        &self.typed
    }

    /// The file part as typed: the text before the first `#`.
    #[must_use]
    pub(crate) fn file(&self) -> &str {
        &self.file
    }

    /// The symbol part as typed: the text after the first `#`.
    #[must_use]
    pub(crate) fn symbol(&self) -> Option<&str> {
        self.symbol.as_deref()
    }

    /// Whether the file part is `ns:path`.
    #[must_use]
    pub(crate) const fn is_namespaced(&self) -> bool {
        self.namespaced
    }
}

impl std::str::FromStr for TargetArg {
    type Err = std::convert::Infallible;

    fn from_str(typed: &str) -> Result<Self, Self::Err> {
        let (file, symbol) = match typed.split_once('#') {
            Some((file, symbol)) => (file, Some(symbol.to_owned())),
            None => (typed, None),
        };
        let namespaced = file
            .split_once(':')
            .is_some_and(|(name, rest)| !rest.is_empty() && name.parse::<NamespaceName>().is_ok());
        Ok(Self {
            typed: typed.to_owned(),
            file: file.to_owned(),
            symbol,
            namespaced,
        })
    }
}

/// How many results `list`, `resolve` and `namespace list` show unless told otherwise.
pub(crate) const DEFAULT_LIMIT: Limit = Limit::at_most(100);

/// How many results a list-style command shows: a count, or everything (`--limit 0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limit(Option<NonZeroUsize>);

impl Limit {
    /// A limit of `count` results; `0` shows everything.
    #[must_use]
    pub(crate) const fn at_most(count: usize) -> Self {
        Self(NonZeroUsize::new(count))
    }

    /// No limit: every result is shown.
    #[cfg(test)]
    pub(crate) const UNLIMITED: Self = Self(None);

    /// The first results of `items`, at most this many.
    pub(crate) fn take<I: IntoIterator>(self, items: I) -> impl Iterator<Item = I::Item> {
        items
            .into_iter()
            .take(self.0.map_or(usize::MAX, NonZeroUsize::get))
    }

    /// Whether `total` results do not all fit under this limit.
    #[must_use]
    pub(crate) fn truncated(self, total: usize) -> bool {
        self.0.is_some_and(|limit| total > limit.get())
    }

    /// The note for a list that showed `shown` of `total` `noun`s, naming how to see them all.
    #[must_use]
    pub(crate) fn truncation_note(shown: usize, total: usize, noun: &str) -> String {
        format!("Showing {shown} of {total} {noun}. Use --limit 0 to show all.")
    }
}

impl std::str::FromStr for Limit {
    type Err = std::num::ParseIntError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse().map(Self::at_most)
    }
}

impl std::fmt::Display for Limit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.map_or(0, NonZeroUsize::get))
    }
}

/// Arguments of `supdoc resolve`.
#[derive(Debug, Args)]
pub(crate) struct ResolveArgs {
    /// The file whose symbols to list
    #[arg(value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub(crate) file: PathBuf,
    /// Check that this symbol resolves in FILE, such as `Config.validate`
    #[arg(value_name = "SYMBOL")]
    pub(crate) symbol: Option<SymbolPath>,
    /// Show at most this many symbols; 0 means no limit
    #[arg(long, env = "SUPDOC_LIMIT", default_value_t = DEFAULT_LIMIT, value_name = "N")]
    pub(crate) limit: Limit,
}

/// Arguments of `supdoc init`.
#[derive(Debug, Args)]
#[command(next_help_heading = "Writing")]
pub(crate) struct InitArgs {
    /// Show what would be written and write nothing
    #[arg(long, env = "SUPDOC_DRY_RUN", value_parser = FalseyValueParser::new())]
    pub(crate) dry_run: bool,
}

/// `supdoc config ...`.
#[derive(Debug, Subcommand)]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub(crate) enum ConfigCommand {
    /// Print the resolved config and which file each value came from (read-only)
    #[command(after_help = CONFIG_SHOW_EXAMPLES)]
    Show,
}

/// `supdoc namespace ...`.
#[derive(Debug, Subcommand)]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub(crate) enum NamespaceCommand {
    /// List namespaces and the directories they map to (read-only)
    #[command(after_help = NAMESPACE_LIST_EXAMPLES)]
    List(NamespaceListArgs),
    /// Rename a namespace in config, supdoc.lock and every link using it (writes; previews unless --yes)
    #[command(after_help = NAMESPACE_RENAME_EXAMPLES)]
    Rename(RenameArgs),
}

/// Arguments of `supdoc namespace list`.
#[derive(Debug, Args)]
pub(crate) struct NamespaceListArgs {
    /// Show at most this many namespaces; 0 means no limit
    #[arg(long, env = "SUPDOC_LIMIT", default_value_t = DEFAULT_LIMIT, value_name = "N")]
    pub(crate) limit: Limit,
}

/// Arguments of `supdoc namespace rename`.
#[derive(Debug, Args)]
pub(crate) struct RenameArgs {
    /// The namespace to rename
    #[arg(value_name = "OLD")]
    pub(crate) old: NamespaceName,
    /// The new name
    #[arg(value_name = "NEW")]
    pub(crate) new: NamespaceName,
    /// Preview or apply.
    #[command(flatten)]
    pub(crate) confirm: Confirm,
}

/// What `config show` and `namespace list` read, handed to the one module that implements both.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Inspect<'a> {
    /// `supdoc config show`.
    ConfigShow,
    /// `supdoc namespace list` with its parsed arguments.
    NamespaceList(&'a NamespaceListArgs),
}

/// Arguments of `supdoc schema`.
#[derive(Debug, Args)]
pub(crate) struct SchemaArgs {
    /// The command whose `--json` output to describe [default: every command]
    #[arg(value_enum, value_name = "COMMAND")]
    pub(crate) command: Option<SchemaTarget>,
}

/// The commands that have JSON output, and so a schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SchemaTarget {
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
pub(crate) struct CompletionsArgs {
    /// The shell to generate completions for
    #[arg(value_enum, value_name = "SHELL")]
    pub(crate) shell: clap_complete::Shell,
}

/// `supdoc skill ...`.
#[derive(Debug, Subcommand)]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub(crate) enum SkillCommand {
    /// Install SKILL.md for agent runtimes (writes; previews unless --yes)
    #[command(after_help = SKILL_INSTALL_EXAMPLES)]
    Install(SkillInstallArgs),
}

/// Arguments of `supdoc skill install`.
#[derive(Debug, Args)]
pub(crate) struct SkillInstallArgs {
    /// Install into the user's home instead of the current project
    #[arg(short, long, env = "SUPDOC_GLOBAL", value_parser = FalseyValueParser::new())]
    pub(crate) global: bool,
    /// Preview or apply.
    #[command(flatten)]
    pub(crate) confirm: Confirm,
}
