//! The tool cartridges supdoc knows how to call on a block, the environment they get, and
//! finding each one's binary on `PATH`.
//!
//! A tool is chosen only by its name in [`CARTRIDGES`]; no path or command ever comes from
//! config.
//!
//! In a checkout supdoc doesn't trust, only [`Tier::Static`] tools run, and each gets the block on
//! standard input, so it opens no other source file. A static tool installed inside the checkout
//! is never used. Each tool still reads its own config files, which can sit outside the project,
//! and its errors can quote them.
//!
//! A [`Tier::Project`] tool runs only once the operator trusts the checkout, so it may be one the
//! checkout installed: it may be found inside the root, in its own bin directories before `PATH`,
//! and may run from the root.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::code_blocks::{Choice, CodeBlocks, CustomCommand, Dialect, Edition, Language};

use super::format::{ParseError, Positions};
use super::fragment::{Kind, Support};
use super::lint;
use super::runner::{Cwd, Failure, Ran};
use crate::config::confine::Boundary;
use crate::path::{self, FixedPath, NativeName, Suffix, fixed};

use super::{
    biome, dockerfmt, eslint, go, hadolint, mbake, prettier, pylint, python, rumdl, rust, shell,
    sqruff, superhtml, tombi, yaml,
};

/// The variables a tool gets from supdoc's own environment, and no others. Windows adds the
/// names a process needs to start (`SystemRoot`, `SystemDrive`, `PATHEXT`, `TEMP`, `TMP`,
/// `COMSPEC`) and `LOCALAPPDATA` and `USERPROFILE`, where it keeps its cache and its home.
#[cfg(not(windows))]
const PASSED: &[&str] = &[
    "PATH",
    "HOME",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "XDG_CACHE_HOME",
];

/// The variables a tool gets from supdoc's own environment, and no others. Windows adds the
/// names a process needs to start (`SystemRoot`, `SystemDrive`, `PATHEXT`, `TEMP`, `TMP`,
/// `COMSPEC`) and `LOCALAPPDATA` and `USERPROFILE`, where it keeps its cache and its home.
#[cfg(windows)]
const PASSED: &[&str] = &[
    "PATH",
    "HOME",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "XDG_CACHE_HOME",
    "SystemRoot",
    "SystemDrive",
    "PATHEXT",
    "TEMP",
    "TMP",
    "COMSPEC",
    "LOCALAPPDATA",
    "USERPROFILE",
];

/// The environment tools get, read once by `main` and passed down like the skill directories.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolEnv {
    /// Each passed variable that is set, with its value, in [`PASSED`] order.
    vars: Vec<(&'static str, OsString)>,
}

impl ToolEnv {
    /// Reads each name in [`PASSED`] through `var`.
    pub(crate) fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Self {
        Self {
            vars: PASSED
                .iter()
                .copied()
                .filter_map(|name| var(name).map(|value| (name, value)))
                .collect(),
        }
    }

    /// Each passed variable that is set, with its value: the whole environment a tool gets.
    pub(crate) fn vars(&self) -> impl Iterator<Item = (&'static str, &OsStr)> {
        self.vars
            .iter()
            .map(|(name, value)| (*name, value.as_os_str()))
    }

    /// `PATH`, when it is set.
    fn path(&self) -> Option<&OsStr> {
        self.var("PATH")
    }

    /// `PATHEXT`, when it is passed and set: only Windows passes it.
    fn pathext(&self) -> Option<&OsStr> {
        self.var("PATHEXT")
    }

    /// The passed variable `name`, when it is set.
    pub(crate) fn var(&self, name: &str) -> Option<&OsStr> {
        self.vars
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_os_str())
    }
}

/// Which check a tool does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Rewrites the block into its formatted form.
    Format,
    /// Reports diagnostics on the block, never rewriting it.
    Lint,
}

/// What a tool loads, which decides whether it may run in a checkout supdoc doesn't trust. The
/// tiers are ordered: a tool above [`Tier::Static`] runs only in a checkout the operator trusts
/// (see `supdoc trust`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Tier {
    /// Reads only the block on stdin and plain-data config: runs in any checkout.
    Static,
    /// Compiles the block, so it needs a trusted project.
    Project,
}

/// What a cartridge loads: its [`Tier`], and for a tool above [`Tier::Static`] what supdoc says
/// when trust holds it back, so a project-tier tool can't be declared without those words. Only a
/// project-tier tool may run from the root or be found inside it, so a static one can't declare
/// either.
#[derive(Debug)]
enum Loads {
    /// Only the block on stdin and plain-data config: [`Tier::Static`]. It runs from the neutral
    /// directory, and a binary the checkout chose is refused.
    Static,
    /// Project code: [`Tier::Project`].
    Project {
        /// What supdoc says when trust holds it back.
        held: Held,
        /// Where it runs: [`Cwd::Root`] for a tool that reads the project as its own command
        /// line would.
        cwd: Cwd<'static>,
        /// The directories, relative to the root, searched for its binary before `PATH`, such as
        /// `node_modules/.bin`: where the project installs its own copy.
        bins: &'static [FixedPath],
    },
}

/// What supdoc says about a project-tier tool's step that trust held back. Each string names the
/// tool, so a reason leads back to the one tool that gave it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Held {
    /// The step's reason when the operator hasn't granted trust: "clippy runs only in the
    /// project tier, which needs `supdoc trust`".
    pub(crate) needs_trust: &'static str,
    /// The step's reason when `SUPDOC_TRUST=0` turned trust off for the run.
    pub(crate) trust_off: &'static str,
    /// [`needs_trust`](Self::needs_trust) as the summary line counts it: "clippy needs trust".
    pub(crate) short: &'static str,
    /// What didn't run and why, for the run's trust warning: "clippy didn't run on Rust blocks:
    /// it compiles them".
    pub(crate) didnt_run: &'static str,
    /// The setting that stops the step, for the warning's help: "`lint = \"off\"` in
    /// `[features.code-blocks.rust.clippy]`".
    pub(crate) off: &'static str,
}

/// A step trust held back: the tool's words, and whether `SUPDOC_TRUST=0` held it rather than a
/// checkout the operator hasn't trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Hold {
    /// The held tool's words.
    pub(crate) held: &'static Held,
    /// Whether the step's reason is [`Held::trust_off`] rather than [`Held::needs_trust`].
    pub(crate) turned_off: bool,
}

/// The hold `reason` records, when it is a project-tier tool's [`Held::needs_trust`] or
/// [`Held::trust_off`]. `None` for any other reason.
#[must_use]
pub(crate) fn hold(reason: &str) -> Option<Hold> {
    CARTRIDGES
        .iter()
        .filter_map(ToolCartridge::held)
        .chain([&CUSTOM_FORMATTER_HELD, &CUSTOM_LINTER_HELD])
        .find_map(|held| {
            (reason == held.needs_trust || reason == held.trust_off).then_some(Hold {
                held,
                turned_off: reason == held.trust_off,
            })
        })
}

/// What a cartridge's argv is built from: the block's place in its doc and its language's
/// settings.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Invocation<'run> {
    /// The doc's absolute path.
    pub(crate) doc: &'run Path,
    /// The block's 1-based position among the blocks of its doc.
    pub(crate) index: usize,
    /// The block's language tag, which picks biome's virtual file extension.
    pub(crate) tag: &'run str,
    /// The Rust edition the block is read as.
    pub(crate) edition: Edition,
    /// The shell dialect the block is read as.
    pub(crate) dialect: Dialect,
    /// The flags the project contributes, each found inside the root: rustfmt's and biome's
    /// `--config-path`, rumdl's `--config` or `--no-config`, clippy's `[lints]` levels. Empty for
    /// every other tool.
    pub(crate) project: &'run [OsString],
    /// The private file a run writes to: where clippy puts the block's metadata, and where
    /// biome lint reads the block from (see [`biome::LintFile`]). `None` names no such path, which
    /// is how a cache key names every run of the same block alike: biome lint then names the
    /// block by its virtual path.
    pub(crate) emit: Option<&'run Path>,
    /// What clippy links the block against: the project build's `--extern` and `-L` flags,
    /// placed right after `--error-format=json`. Empty when the block compiles alone, and for
    /// every other tool.
    pub(crate) externs: &'run [OsString],
    /// The kind the block's `fragment(<kind>)` declares, which names the rules a tool relaxes
    /// for it (see [`Support::relax`]). `None` for a block checked as a whole file.
    pub(crate) fragment: Option<Kind>,
}

/// How a cartridge reads a finished run, which also says which step it does.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Reader {
    /// A formatter: its syntax-error report in stderr, `None` for any other stderr. The format
    /// step compares its stdout with the block (see [`super::format::judge`]).
    Format(fn(&str) -> Option<ParseError>),
    /// A linter: its verdict on a run over the checked text, told whether that text frames the
    /// block.
    Lint(fn(&str, bool, Result<Ran, Failure>) -> lint::Verdict),
}

/// One tool supdoc knows how to call safely: everything the pipeline needs to look it up, run it
/// on a block and read the result.
///
/// A struct of fn pointers rather than a trait: the seven cartridges are constant data in one
/// table, chosen by name, with no state of their own, so a trait would add a unit type and an
/// `impl` per tool and `dyn` dispatch while giving nothing a fn pointer doesn't.
#[derive(Debug)]
pub(crate) struct ToolCartridge {
    /// The name config uses for it.
    pub(crate) name: &'static str,
    /// The language whose blocks it checks.
    pub(crate) language: Language,
    /// The binary looked up on `PATH`.
    pub(crate) binary: &'static str,
    /// What it loads.
    loads: Loads,
    /// The command that installs it.
    pub(crate) install: &'static str,
    /// The config files it reads, by name at the project root: each one's content is part of
    /// its cached results' key.
    pub(crate) config_files: &'static [FixedPath],
    /// The values its step's mode accepts, in config and in a block attribute.
    pub(crate) modes: &'static [&'static str],
    /// The fragment kinds it checks, and how.
    pub(crate) fragments: &'static [(Kind, Support)],
    /// Whether a block tagged `tag` can hold a fragment of `kind` at all, whatever `fragments`
    /// lists: biome reads a markup element's children only in a JSX flavor.
    holds: fn(&str, Kind) -> bool,
    /// The variables it gets beyond [`ToolEnv`]'s, each set to the value held.
    vars: &'static [(&'static str, &'static str)],
    /// How far the positions in its syntax-error report on `input` are the block's own: gofmt
    /// reports a fragment's in a wrapper of its own.
    positions: fn(&str) -> Positions,
    /// Its arguments for a block.
    argv: fn(&Invocation<'_>) -> Vec<OsString>,
    /// How a run is read.
    reader: Reader,
}

impl ToolCartridge {
    /// The tier it needs.
    #[must_use]
    pub(crate) const fn tier(&self) -> Tier {
        match self.loads {
            Loads::Static => Tier::Static,
            Loads::Project { .. } => Tier::Project,
        }
    }

    /// Where it runs: [`Cwd::Neutral`] for a [`Tier::Static`] tool.
    #[must_use]
    pub(crate) const fn cwd(&self) -> Cwd<'static> {
        match self.loads {
            Loads::Static => Cwd::Neutral,
            Loads::Project { cwd, .. } => cwd,
        }
    }

    /// The root-relative directories searched for its binary before `PATH`: none for a
    /// [`Tier::Static`] tool.
    #[must_use]
    pub(crate) const fn bins(&self) -> &'static [FixedPath] {
        match self.loads {
            Loads::Static => &[],
            Loads::Project { bins, .. } => bins,
        }
    }

    /// Whether its runs may be cached. Not for a tool run from the root: it reads the project as
    /// its own command line would, files no cache key names.
    #[must_use]
    pub(crate) const fn cacheable(&self) -> bool {
        !matches!(self.cwd(), Cwd::Root)
    }

    /// What supdoc says when trust holds it back. `None` for a [`Tier::Static`] tool, which trust
    /// never holds.
    #[must_use]
    pub(crate) const fn held(&self) -> Option<&Held> {
        match &self.loads {
            Loads::Static => None,
            Loads::Project { held, .. } => Some(held),
        }
    }

    /// The check it does.
    #[must_use]
    pub(crate) const fn step(&self) -> Step {
        match self.reader {
            Reader::Format(_) => Step::Format,
            Reader::Lint(_) => Step::Lint,
        }
    }

    /// Whether it checks a fragment of `kind` in a block tagged `tag`.
    #[must_use]
    pub(crate) fn supports(&self, kind: Kind, tag: &str) -> bool {
        self.fragments.iter().any(|(listed, _)| *listed == kind) && (self.holds)(tag, kind)
    }

    /// Its arguments for the block `invocation` describes.
    #[must_use]
    pub(crate) fn args(&self, invocation: &Invocation<'_>) -> Vec<OsString> {
        (self.argv)(invocation)
    }

    /// The variables it gets beyond [`ToolEnv`]'s.
    #[must_use]
    pub(crate) fn vars(&self) -> Vec<(&'static str, &'static OsStr)> {
        self.vars
            .iter()
            .map(|(name, value)| (*name, OsStr::new(*value)))
            .collect()
    }

    /// How far the positions in its syntax-error report on `input` are the block's own, which
    /// decides how the report places the error in the doc.
    #[must_use]
    pub(crate) fn positions(&self, input: &str) -> Positions {
        (self.positions)(input)
    }

    /// Its syntax-error report in `stderr`. `None` for any other stderr, and always for a
    /// linter.
    #[must_use]
    pub(crate) fn parse_error(&self, stderr: &str) -> Option<ParseError> {
        match self.reader {
            Reader::Format(read) => read(stderr),
            Reader::Lint(_) => None,
        }
    }

    /// Its lint verdict on `result`, a run over `input`, which frames the block when `framed`.
    /// A formatter reports no diagnostics, so it is judged on its exit alone.
    #[must_use]
    pub(crate) fn lint_verdict(
        &self,
        input: &str,
        framed: bool,
        result: Result<Ran, Failure>,
    ) -> lint::Verdict {
        match self.reader {
            Reader::Lint(read) => read(input, framed, result),
            Reader::Format(_) => lint::judge(result, |_| None),
        }
    }
}

/// A command the config wrote as a language's `formatter` or `linter`, held as a [`Tool`]: the
/// program and its arguments as written, for one language's step.
#[derive(Debug)]
pub(crate) struct CustomTool {
    /// The command as config validated it.
    command: CustomCommand,
    /// The language whose blocks it checks.
    language: Language,
    /// The step it does.
    step: Step,
}

impl CustomTool {
    /// The command config wrote for `language`'s `step`.
    #[must_use]
    pub(crate) const fn new(command: CustomCommand, language: Language, step: Step) -> Self {
        Self {
            command,
            language,
            step,
        }
    }

    /// The command as config validated it.
    #[must_use]
    pub(crate) const fn command(&self) -> &CustomCommand {
        &self.command
    }

    /// The language whose blocks it checks.
    #[must_use]
    pub(crate) const fn language(&self) -> Language {
        self.language
    }

    /// The step it does.
    #[must_use]
    pub(crate) const fn step(&self) -> Step {
        self.step
    }

    /// How a message names it: "custom formatter command `<program>`".
    #[must_use]
    pub(crate) fn shown(&self) -> String {
        format!(
            "{} `{}`",
            self.kind(),
            self.command.program().escape_debug()
        )
    }

    /// What is wrong when its program wasn't found: "isn't on PATH" for a bare name, else that
    /// no executable file is at the path, "under the root" for a relative one.
    #[must_use]
    pub(crate) fn missing(&self) -> &'static str {
        let program = self.command.program();
        if !program.contains('/') {
            "isn't on PATH"
        } else if Path::new(program).is_absolute() {
            "doesn't exist as an executable file"
        } else {
            "doesn't exist under the root as an executable file"
        }
    }

    /// How its messages name it: "custom formatter command" or "custom linter command".
    #[must_use]
    pub(crate) const fn kind(&self) -> &'static str {
        match self.step {
            Step::Format => "custom formatter command",
            Step::Lint => "custom linter command",
        }
    }
}

/// What supdoc says when trust holds a custom formatter back. Its reasons stay static, as every
/// block reason is; the trust warning names each held program (see [`CustomTool::command`]).
pub(crate) static CUSTOM_FORMATTER_HELD: Held = Held {
    needs_trust: "a custom formatter command runs only in the project tier, which needs \
                  `supdoc trust`",
    trust_off: "a custom formatter command runs only in the project tier, and `SUPDOC_TRUST=0` \
                turns trust off for this run",
    short: "custom formatter command needs trust",
    didnt_run: "custom formatter commands didn't run: the checkout chose them",
    off: "a known tool's name as the `formatter`",
};

/// What supdoc says when trust holds a custom linter back, as [`CUSTOM_FORMATTER_HELD`] does.
pub(crate) static CUSTOM_LINTER_HELD: Held = Held {
    needs_trust: "a custom linter command runs only in the project tier, which needs \
                  `supdoc trust`",
    trust_off: "a custom linter command runs only in the project tier, and `SUPDOC_TRUST=0` \
                turns trust off for this run",
    short: "custom linter command needs trust",
    didnt_run: "custom linter commands didn't run: the checkout chose them",
    off: "a known tool's name as the `linter`",
};

/// The tool selected for a step: one of supdoc's [`CARTRIDGES`], or a command the config wrote.
/// Everything from selection to the verdict holds this handle, so either kind can flow through.
#[derive(Debug, Clone)]
pub(crate) enum Tool {
    /// One of supdoc's cartridges.
    Cartridge(&'static ToolCartridge),
    /// A custom command.
    Custom(Arc<CustomTool>),
}

impl Tool {
    /// Its bare name: a cartridge's config name, or a custom command's program. Messages name a
    /// tool by [`Self::label`] or [`Self::shown`], which keep a custom command apart from a
    /// cartridge.
    #[must_use]
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Cartridge(cartridge) => cartridge.name,
            Self::Custom(custom) => custom.command.program(),
        }
    }

    /// How a message names it in passing: a cartridge's config name, or "custom formatter
    /// command `<program>`", which no cartridge's name can be mistaken for.
    #[must_use]
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Cartridge(cartridge) => cartridge.name.to_owned(),
            Self::Custom(custom) => custom.shown(),
        }
    }

    /// How a block's reason names it: a cartridge's config name in backticks, or "custom
    /// formatter command `<program>`".
    #[must_use]
    pub(crate) fn shown(&self) -> String {
        match self {
            Self::Cartridge(cartridge) => format!("`{}`", cartridge.name),
            Self::Custom(custom) => custom.shown(),
        }
    }

    /// Why its step didn't run when its binary wasn't found: "`<binary>` isn't on PATH" (naming any
    /// bin dirs searched first, see [`searched`]), or, for
    /// a custom command whose program is a path, that no executable file is there.
    #[must_use]
    pub(crate) fn missing(&self) -> String {
        match self {
            Self::Cartridge(cartridge) => format!(
                "`{}` isn't {}",
                cartridge.binary,
                searched(cartridge.bins())
            ),
            Self::Custom(custom) => format!("{} {}", custom.shown(), custom.missing()),
        }
    }

    /// Why its step didn't run when the binary found is inside the checkout, which only a
    /// static-tier cartridge is refused for.
    #[must_use]
    pub(crate) fn inside_root(&self) -> String {
        match self {
            Self::Cartridge(cartridge) => format!("`{}` is inside the checkout", cartridge.binary),
            Self::Custom(custom) => format!("{} is inside the checkout", custom.shown()),
        }
    }

    /// The binary looked up: a cartridge's, or a custom command's program.
    #[must_use]
    pub(crate) fn binary(&self) -> &str {
        match self {
            Self::Cartridge(cartridge) => cartridge.binary,
            Self::Custom(custom) => custom.command.program(),
        }
    }

    /// The root-relative directories searched for its binary before `PATH`: a cartridge's, and
    /// none for a custom command.
    #[must_use]
    pub(crate) const fn bins(&self) -> &'static [FixedPath] {
        match self {
            Self::Cartridge(cartridge) => cartridge.bins(),
            Self::Custom(_) => &[],
        }
    }

    /// The tier it needs: every custom command needs [`Tier::Project`].
    #[must_use]
    pub(crate) const fn tier(&self) -> Tier {
        match self {
            Self::Cartridge(cartridge) => cartridge.tier(),
            Self::Custom(_) => Tier::Project,
        }
    }

    /// What supdoc says when trust holds it back. `None` for a [`Tier::Static`] cartridge, which
    /// trust never holds.
    #[must_use]
    pub(crate) fn held(&self) -> Option<&'static Held> {
        match self {
            Self::Cartridge(cartridge) => cartridge.held(),
            Self::Custom(custom) => Some(match custom.step {
                Step::Format => &CUSTOM_FORMATTER_HELD,
                Step::Lint => &CUSTOM_LINTER_HELD,
            }),
        }
    }

    /// The check it does.
    #[must_use]
    pub(crate) fn step(&self) -> Step {
        match self {
            Self::Cartridge(cartridge) => cartridge.step(),
            Self::Custom(custom) => custom.step,
        }
    }

    /// The values its step's mode accepts, in config and in a block attribute.
    #[must_use]
    pub(crate) const fn modes(&self) -> &'static [&'static str] {
        match self {
            Self::Cartridge(cartridge) => cartridge.modes,
            Self::Custom(_) => FULL_OR_OFF,
        }
    }

    /// Whether it checks a fragment of `kind` in a block tagged `tag`: a custom command checks
    /// none.
    #[must_use]
    pub(crate) fn supports(&self, kind: Kind, tag: &str) -> bool {
        match self {
            Self::Cartridge(cartridge) => cartridge.supports(kind, tag),
            Self::Custom(_) => false,
        }
    }

    /// The cartridge it is. `None` for a custom command.
    #[must_use]
    pub(crate) const fn cartridge(&self) -> Option<&'static ToolCartridge> {
        match self {
            Self::Cartridge(cartridge) => Some(cartridge),
            Self::Custom(_) => None,
        }
    }

    /// Whether it is the cartridge named `name`. Never a custom command, whatever its program.
    #[must_use]
    pub(crate) fn is(&self, name: &str) -> bool {
        self.cartridge()
            .is_some_and(|cartridge| cartridge.name == name)
    }

    /// Whether it is a cartridge that runs `binary`. Never a custom command, whatever its
    /// program, so no custom command inherits a cartridge's setup.
    #[must_use]
    pub(crate) fn runs(&self, binary: &str) -> bool {
        self.cartridge()
            .is_some_and(|cartridge| cartridge.binary == binary)
    }
}

/// Where a binary was looked for: `on PATH`, after each of the root's `bins` when it has any, as
/// `in .venv/bin or on PATH`.
#[must_use]
pub(crate) fn searched(bins: &[FixedPath]) -> String {
    match bins.split_last() {
        None => "on PATH".to_owned(),
        Some((last, [])) => format!("in {} or on PATH", last.as_str()),
        Some((last, rest)) => format!(
            "in {}, {} or on PATH",
            rest.iter()
                .map(FixedPath::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            last.as_str()
        ),
    }
}

/// The command that installs a tombi supdoc's checks are verified against (see
/// [`tombi::VERIFIED`]).
pub(crate) const TOMBI_INSTALL: &str = "pipx install 'tombi==1.10.*'";

/// The command that installs an mbake supdoc's checks are verified against (see
/// [`mbake::VERIFIED`]).
pub(crate) const MBAKE_INSTALL: &str = "pipx install 'mbake==1.5.*'";

/// How to install a dockerfmt supdoc's checks are verified against (see
/// [`dockerfmt::VERIFIED`]): dockerfmt ships a binary per platform, and no package manager supdoc
/// can name pins its version.
pub(crate) const DOCKERFMT_INSTALL: &str = "download dockerfmt 0.6.0 from https://github.com/reteps/dockerfmt/releases/tag/v0.6.0 \
     and put it on PATH";

/// How to install a superhtml supdoc's checks are verified against (see [`superhtml::VERIFIED`]):
/// superhtml ships a binary per platform, and no package manager supdoc can name pins its version.
pub(crate) const SUPERHTML_INSTALL: &str = "download superhtml 0.7.1 from https://github.com/kristoff-it/superhtml/releases/tag/v0.7.1 \
     and put it on PATH";

/// How to install a hadolint supdoc's checks are verified against (see [`hadolint::VERIFIED`]):
/// hadolint ships a binary per platform, and no package manager supdoc can name pins its version.
pub(crate) const HADOLINT_INSTALL: &str = "download hadolint 2.15.1 from https://github.com/hadolint/hadolint/releases/tag/v2.15.1 \
     and put it on PATH";

/// How to install a sqruff supdoc's checks are verified against (see [`sqruff::VERIFIED`]):
/// sqruff ships a binary per platform with a `.sha256` beside it.
pub(crate) const SQRUFF_INSTALL: &str = "download sqruff 0.44.0 from https://github.com/quarylabs/sqruff/releases/tag/v0.44.0, \
     check it against its `.sha256`, and put it on PATH";

/// The values every cartridge's mode accepts today.
const FULL_OR_OFF: &[&str] = &["full", "off"];

/// rustfmt's kinds: a method without its `impl` is an item, so `members` parses as written.
/// Statements and an expression are not items, so it reads both as the body of a function
/// ([`RUSTFMT_BODY`]).
const RUSTFMT_FRAGMENTS: &[(Kind, Support)] = &[
    (Kind::Statements, RUSTFMT_BODY),
    (Kind::Expression, RUSTFMT_BODY),
    (Kind::Members, Support::NATIVE),
];

/// The frame rustfmt reads a function body in, and prints back unchanged: a function no author
/// would name, with no parameters and no return type, so an expression is its tail.
const RUSTFMT_BODY: Support = Support::Framed {
    before: "fn supdoc_fragment() {",
    after: "}",
    out_before: "fn supdoc_fragment() {",
    out_after: "}",
};

/// clippy's kinds, each read inside a frame that draws no lint of its own under clippy's default
/// lints, so no lint is ever left off for a whole block. The block compiles as a library, where
/// the frame's private items are never reported unused. Statements are the body of a function
/// with no parameters and no return type, so a `?` in them is a true error, never one the
/// block's content chose a `Result` for. An expression is the tail of a function returning
/// `impl Sized`, so a value of any type reads as written. Members are the `impl` of a unit
/// struct the frame declares, so a field read on `self` is a true error. clippy prints no code.
/// The frame's names are the ones [`super::rust::lint_verdict`] keeps out of clippy's messages.
const CLIPPY_FRAGMENTS: &[(Kind, Support)] = &[
    (Kind::Statements, framed("fn supdoc_fragment() {", "}")),
    (
        Kind::Expression,
        framed("fn supdoc_fragment() -> impl Sized {", "}"),
    ),
    (
        Kind::Members,
        framed("struct SupdocFragment; impl SupdocFragment {", "}"),
    ),
];

/// shfmt's and shellcheck's kinds: any command list is a program, and one command is a list.
const SHELL_FRAGMENTS: &[(Kind, Support)] = &[
    (Kind::Statements, Support::NATIVE),
    (Kind::Expression, Support::NATIVE),
];

/// ruff format's and black's kinds: statements, an expression and a method without its class
/// all parse as a module (a top-level `return` included). Map entries without their braces don't.
const PYTHON_FORMAT_FRAGMENTS: &[(Kind, Support)] = &[
    (Kind::Statements, Support::NATIVE),
    (Kind::Expression, Support::NATIVE),
    (Kind::Members, Support::NATIVE),
];

/// ruff check's kinds, as [`PYTHON_FORMAT_FRAGMENTS`]. A function body outside its function
/// trips `return` outside a function (F706), `yield` or `await` outside a function (F704) and
/// `await` outside an async function (PLE1142), so `statements` leaves those off.
const RUFF_FRAGMENTS: &[(Kind, Support)] = &[
    (
        Kind::Statements,
        Support::Native {
            relax: &["F706", "F704", "PLE1142"],
        },
    ),
    (Kind::Expression, Support::NATIVE),
    (Kind::Members, Support::NATIVE),
];

/// biome lint's kinds, each read inside a frame that draws no diagnostic of its own under
/// biome's default rules, so no rule is ever left off for a whole block: statements as an
/// `async` function's body (so `await` reads as written) that awaits a promise of its own (so
/// `suspicious/useAwait` never fires on the frame), members as a class's that has an instance
/// method of its own using `this` (so `complexity/noStaticOnlyClass` never fires on the frame),
/// an expression in parentheses (an assignment included), and map entries and a markup
/// element's children (only in a `tsx` or `jsx` block, see [`biome::holds`]) as the value of a
/// `const`, children in a `<div>`, never a `<>`, which biome calls unnecessary around one child.
/// biome lint prints no code, so the output lines are the input's. A JavaScript block reads
/// these; a TypeScript block reads [`BIOME_LINT_TS_FRAGMENTS`].
const BIOME_LINT_FRAGMENTS: &[(Kind, Support)] = &[
    (Kind::Statements, framed(BIOME_STATEMENTS, "}")),
    (Kind::Expression, framed("(", ")")),
    (Kind::Entries, framed(BIOME_ENTRIES, "};")),
    (Kind::Children, framed(BIOME_CHILDREN, "</div>;")),
    (Kind::Members, framed(BIOME_MEMBERS, "}")),
];

/// [`BIOME_LINT_FRAGMENTS`] for a TypeScript block, whose members frame is an `abstract`
/// class so an abstract member reads as written. JavaScript has no abstract class, so a
/// JavaScript block keeps the plain one.
const BIOME_LINT_TS_FRAGMENTS: &[(Kind, Support)] = &[
    (Kind::Statements, framed(BIOME_STATEMENTS, "}")),
    (Kind::Expression, framed("(", ")")),
    (Kind::Entries, framed(BIOME_ENTRIES, "};")),
    (Kind::Children, framed(BIOME_CHILDREN, "</div>;")),
    (Kind::Members, framed(BIOME_TS_MEMBERS, "}")),
];

/// biome format's kinds, each read inside biome lint's frame for it, so a block is formatted in
/// exactly the code it is linted in. biome prints each frame back in its own layout, a tab
/// standing for one indent unit: the statements frame's `await` and the members frame's method
/// each on lines of their own, and a children frame's multi-line `<div>` in parentheses, which
/// puts the block one unit deeper than the closing `</div>` (see [`super::format::depth`]). An
/// expression isn't listed: biome joins a short parenthesized one onto the frame's lines. A
/// JavaScript block reads these; a TypeScript block reads [`BIOME_FORMAT_TS_FRAGMENTS`].
const BIOME_FORMAT_FRAGMENTS: &[(Kind, Support)] = &[
    BIOME_FORMAT_STATEMENTS,
    BIOME_FORMAT_ENTRIES,
    BIOME_FORMAT_CHILDREN,
    (
        Kind::Members,
        printed(
            BIOME_MEMBERS,
            "}",
            "class SupdocFragment$ {\n\tsupdocFrame$() {\n\t\treturn this;\n\t}",
            "}",
        ),
    ),
];

/// [`BIOME_FORMAT_FRAGMENTS`] for a TypeScript block, with biome lint's `abstract` members frame.
const BIOME_FORMAT_TS_FRAGMENTS: &[(Kind, Support)] = &[
    BIOME_FORMAT_STATEMENTS,
    BIOME_FORMAT_ENTRIES,
    BIOME_FORMAT_CHILDREN,
    (
        Kind::Members,
        printed(
            BIOME_TS_MEMBERS,
            "}",
            "abstract class SupdocFragment$ {\n\tsupdocFrame$() {\n\t\treturn this;\n\t}",
            "}",
        ),
    ),
];

/// biome format's statements frame, in both languages.
const BIOME_FORMAT_STATEMENTS: (Kind, Support) = (
    Kind::Statements,
    printed(
        BIOME_STATEMENTS,
        "}",
        "async function supdocFragment$() {\n\tawait Promise.resolve();",
        "}",
    ),
);

/// biome format's entries frame, in both languages: printed back as written.
const BIOME_FORMAT_ENTRIES: (Kind, Support) = (Kind::Entries, framed(BIOME_ENTRIES, "};"));

/// biome format's children frame, in both languages.
const BIOME_FORMAT_CHILDREN: (Kind, Support) = (
    Kind::Children,
    printed(
        BIOME_CHILDREN,
        "</div>;",
        "const supdocFragment$ = (\n\t<div>",
        "\t</div>\n);",
    ),
);

/// The line biome's statements frame opens with.
const BIOME_STATEMENTS: &str = "async function supdocFragment$() { await Promise.resolve();";

/// The line biome's entries frame opens with.
const BIOME_ENTRIES: &str = "const supdocFragment$ = {";

/// The line biome's children frame opens with.
const BIOME_CHILDREN: &str = "const supdocFragment$ = <div>";

/// The line biome's members frame opens with in a JavaScript block.
const BIOME_MEMBERS: &str = "class SupdocFragment$ { supdocFrame$() { return this; }";

/// The line biome's members frame opens with in a TypeScript block.
const BIOME_TS_MEMBERS: &str = "abstract class SupdocFragment$ { supdocFrame$() { return this; }";

/// biome's kind for a JSON block, in format and lint alike: an object's entries, read inside the
/// object's braces, which biome prints back as written with each entry one indent unit deep.
const BIOME_JSON_FRAGMENTS: &[(Kind, Support)] = &[(Kind::Entries, framed("{", "}"))];

/// A frame of `before` and `after` that a formatter prints back as `out_before` and
/// `out_after`, a tab in them standing for one indent unit.
const fn printed(
    before: &'static str,
    after: &'static str,
    out_before: &'static str,
    out_after: &'static str,
) -> Support {
    Support::Framed {
        before,
        after,
        out_before,
        out_after,
    }
}

/// A frame of `before` and `after` that a linter reads, or a formatter prints back as written.
const fn framed(before: &'static str, after: &'static str) -> Support {
    Support::Framed {
        before,
        after,
        out_before: before,
        out_after: after,
    }
}

/// How far a tool's syntax-error positions on `input` are the block's own: wholly, for a tool
/// that reads the block as written.
const fn own_positions(_input: &str) -> Positions {
    Positions::Own
}

/// gofmt's kinds: it tries the block as a file, then as a declaration list, then as a statement
/// list (an expression is one), wrapping it itself, so each reads as written.
const GO_FRAGMENTS: &[(Kind, Support)] = &[
    (Kind::Statements, Support::NATIVE),
    (Kind::Expression, Support::NATIVE),
    (Kind::Declarations, Support::NATIVE),
];

/// go vet's kinds: it reads only a whole file, so declarations are read behind a package clause
/// and statements inside a function too, each frame on one line. An expression is no statement
/// Go accepts alone (its value would be unused), so go vet doesn't list it.
const GO_VET_FRAGMENTS: &[(Kind, Support)] = &[
    (Kind::Statements, framed(GO_VET_STATEMENTS, "}")),
    (Kind::Declarations, framed(GO_VET_PACKAGE, "")),
];

/// The package clause go vet's frames open with.
const GO_VET_PACKAGE: &str = "package supdocfragment";

/// The line go vet's statements frame opens with: [`GO_VET_PACKAGE`] and a function.
const GO_VET_STATEMENTS: &str = "package supdocfragment; func supdocFragment() {";

/// Whether a block tagged `tag` can hold a fragment of `kind`: always, for a tool whose
/// `fragments` alone say.
const fn every_tag(_tag: &str, _kind: Kind) -> bool {
    true
}

/// The rules `fragments` leaves off for a block of `kind`: none for a whole file or a kind it
/// doesn't list.
fn relaxed(fragments: &[(Kind, Support)], kind: Option<Kind>) -> &'static [&'static str] {
    fragments
        .iter()
        .find(|(listed, _)| Some(*listed) == kind)
        .map_or(&[], |(_, support)| support.relax())
}

/// The known tools. A language's first cartridge for a step is its default for that step.
pub(crate) static CARTRIDGES: [ToolCartridge; 36] = [
    ToolCartridge {
        name: "rustfmt",
        language: Language::Rust,
        binary: "rustfmt",
        loads: Loads::Static,
        install: "rustup component add rustfmt",
        config_files: &fixed::RUSTFMT_TOOL_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: RUSTFMT_FRAGMENTS,
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: rustfmt_args,
        reader: Reader::Format(rust::parse_error),
    },
    ToolCartridge {
        name: "shfmt",
        language: Language::Shell,
        binary: "shfmt",
        loads: Loads::Static,
        install: "go install mvdan.cc/sh/v3/cmd/shfmt@latest",
        config_files: &fixed::EDITORCONFIG_TOOL_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: SHELL_FRAGMENTS,
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: shfmt_args,
        reader: Reader::Format(shell::parse_error),
    },
    ToolCartridge {
        name: "ruff-format",
        language: Language::Python,
        binary: "ruff",
        loads: Loads::Static,
        install: "pipx install ruff",
        config_files: &fixed::RUFF_TOOL_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: PYTHON_FORMAT_FRAGMENTS,
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: ruff_format_args,
        reader: Reader::Format(python::ruff_parse_error),
    },
    ToolCartridge {
        name: "black",
        language: Language::Python,
        binary: "black",
        loads: Loads::Static,
        install: "pipx install black",
        config_files: &fixed::PYPROJECT_TOOL_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: PYTHON_FORMAT_FRAGMENTS,
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: black_args,
        reader: Reader::Format(python::black_parse_error),
    },
    ToolCartridge {
        name: "ruff",
        language: Language::Python,
        binary: "ruff",
        loads: Loads::Static,
        install: "pipx install ruff",
        config_files: &fixed::RUFF_TOOL_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: RUFF_FRAGMENTS,
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: ruff_args,
        reader: Reader::Lint(python::lint_verdict),
    },
    ToolCartridge {
        name: "shellcheck",
        language: Language::Shell,
        binary: "shellcheck",
        loads: Loads::Static,
        install: "cabal install ShellCheck",
        config_files: &fixed::SHELLCHECK_TOOL_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: SHELL_FRAGMENTS,
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: shellcheck_args,
        reader: Reader::Lint(shell::lint_verdict),
    },
    ToolCartridge {
        name: "clippy",
        language: Language::Rust,
        binary: "clippy-driver",
        loads: Loads::Project {
            held: Held {
                needs_trust: "clippy runs only in the project tier, which needs `supdoc trust`",
                trust_off: "clippy runs only in the project tier, and `SUPDOC_TRUST=0` turns \
                            trust off for this run",
                short: "clippy needs trust",
                didnt_run: "clippy didn't run on Rust blocks: it compiles them",
                off: "`lint = \"off\"` in `[features.code-blocks.rust.clippy]`",
            },
            // No cwd lookup: the block compiles alone or against supdoc's build of the crate.
            cwd: Cwd::Neutral,
            bins: &[],
        },
        install: "rustup component add clippy",
        config_files: &fixed::CLIPPY_TOOL_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: CLIPPY_FRAGMENTS,
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: clippy_args,
        reader: Reader::Lint(rust::lint_verdict),
    },
    ToolCartridge {
        name: "biome-format",
        language: Language::TypeScript,
        binary: "biome",
        loads: Loads::Static,
        install: "npm install --global @biomejs/biome",
        config_files: &fixed::BIOME_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: BIOME_FORMAT_TS_FRAGMENTS,
        holds: biome::holds,
        vars: &[],
        positions: own_positions,
        argv: biome_format_args,
        reader: Reader::Format(biome::parse_error),
    },
    ToolCartridge {
        name: "biome-format",
        language: Language::JavaScript,
        binary: "biome",
        loads: Loads::Static,
        install: "npm install --global @biomejs/biome",
        config_files: &fixed::BIOME_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: BIOME_FORMAT_FRAGMENTS,
        holds: biome::holds,
        vars: &[],
        positions: own_positions,
        argv: biome_format_args,
        reader: Reader::Format(biome::parse_error),
    },
    ToolCartridge {
        name: "biome-format",
        language: Language::Json,
        binary: "biome",
        loads: Loads::Static,
        install: "npm install --global @biomejs/biome",
        config_files: &fixed::BIOME_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: BIOME_JSON_FRAGMENTS,
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: biome_format_args,
        reader: Reader::Format(biome::parse_error),
    },
    ToolCartridge {
        name: "biome-format",
        language: Language::Css,
        binary: "biome",
        loads: Loads::Static,
        install: "npm install --global @biomejs/biome",
        config_files: &fixed::BIOME_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: biome_format_args,
        reader: Reader::Format(biome::parse_error),
    },
    ToolCartridge {
        name: "biome-lint",
        language: Language::TypeScript,
        binary: "biome",
        loads: Loads::Static,
        install: "npm install --global @biomejs/biome",
        config_files: &fixed::BIOME_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: BIOME_LINT_TS_FRAGMENTS,
        holds: biome::holds,
        vars: &[],
        positions: own_positions,
        argv: biome_lint_args,
        reader: Reader::Lint(biome::lint_verdict),
    },
    ToolCartridge {
        name: "biome-lint",
        language: Language::JavaScript,
        binary: "biome",
        loads: Loads::Static,
        install: "npm install --global @biomejs/biome",
        config_files: &fixed::BIOME_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: BIOME_LINT_FRAGMENTS,
        holds: biome::holds,
        vars: &[],
        positions: own_positions,
        argv: biome_lint_args,
        reader: Reader::Lint(biome::lint_verdict),
    },
    ToolCartridge {
        name: "biome-lint",
        language: Language::Json,
        binary: "biome",
        loads: Loads::Static,
        install: "npm install --global @biomejs/biome",
        config_files: &fixed::BIOME_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: BIOME_JSON_FRAGMENTS,
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: biome_lint_args,
        reader: Reader::Lint(biome::lint_verdict),
    },
    ToolCartridge {
        name: "biome-lint",
        language: Language::Css,
        binary: "biome",
        loads: Loads::Static,
        install: "npm install --global @biomejs/biome",
        config_files: &fixed::BIOME_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: biome_lint_args,
        reader: Reader::Lint(biome::lint_verdict),
    },
    ToolCartridge {
        name: "rumdl-format",
        language: Language::Markdown,
        binary: "rumdl",
        loads: Loads::Static,
        install: "pipx install rumdl",
        config_files: &fixed::RUMDL_TOOL_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: rumdl_format_args,
        reader: Reader::Format(rumdl::parse_error),
    },
    ToolCartridge {
        name: "rumdl",
        language: Language::Markdown,
        binary: "rumdl",
        loads: Loads::Static,
        install: "pipx install rumdl",
        config_files: &fixed::RUMDL_TOOL_CONFIG_FILES,
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: rumdl_args,
        reader: Reader::Lint(rumdl::lint_verdict),
    },
    ToolCartridge {
        name: "tombi-format",
        language: Language::Toml,
        binary: "tombi",
        loads: Loads::Static,
        install: TOMBI_INSTALL,
        // tombi reads only supdoc's private copy, named by its hash in the args and env.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: tombi_format_args,
        reader: Reader::Format(tombi::parse_error),
    },
    ToolCartridge {
        name: "tombi-lint",
        language: Language::Toml,
        binary: "tombi",
        loads: Loads::Static,
        install: TOMBI_INSTALL,
        // tombi reads only supdoc's private copy, named by its hash in the args and env.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: tombi_lint_args,
        reader: Reader::Lint(tombi::lint_verdict),
    },
    ToolCartridge {
        name: "dockerfmt",
        language: Language::Dockerfile,
        binary: "dockerfmt",
        loads: Loads::Static,
        install: DOCKERFMT_INSTALL,
        // dockerfmt reads `.editorconfig` only for a file path, never for stdin.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: dockerfmt_args,
        reader: Reader::Format(dockerfmt::parse_error),
    },
    ToolCartridge {
        name: "hadolint",
        language: Language::Dockerfile,
        binary: "hadolint",
        loads: Loads::Static,
        install: HADOLINT_INSTALL,
        // hadolint reads only supdoc's private copy, named by its hash in the args.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: hadolint_args,
        reader: Reader::Lint(hadolint::lint_verdict),
    },
    ToolCartridge {
        name: "sqruff-format",
        language: Language::Sql,
        binary: "sqruff",
        loads: Loads::Static,
        install: SQRUFF_INSTALL,
        // sqruff reads only supdoc's private copy, named by its hash in the args.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: sqruff_format_args,
        reader: Reader::Format(sqruff::parse_error),
    },
    ToolCartridge {
        name: "mbake",
        language: Language::Makefile,
        binary: "mbake",
        loads: Loads::Static,
        install: MBAKE_INSTALL,
        // mbake reads only supdoc's private copy, named by its hash in the args.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: mbake_args,
        reader: Reader::Format(mbake::parse_error),
    },
    ToolCartridge {
        name: "superhtml",
        language: Language::Html,
        binary: "superhtml",
        loads: Loads::Static,
        install: SUPERHTML_INSTALL,
        // superhtml reads no config file and no environment variable.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: superhtml_args,
        reader: Reader::Format(superhtml::parse_error),
    },
    ToolCartridge {
        name: "yamlfmt",
        language: Language::Yaml,
        binary: "yamlfmt",
        loads: Loads::Static,
        install: "go install github.com/google/yamlfmt/cmd/yamlfmt@v0.21.0",
        // yamlfmt reads only supdoc's private copy, named by its hash in the args.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: yamlfmt_args,
        reader: Reader::Format(yaml::parse_error),
    },
    ToolCartridge {
        name: "yamllint",
        language: Language::Yaml,
        binary: "yamllint",
        loads: Loads::Static,
        install: "pipx install 'yamllint==1.38.0'",
        // yamllint reads only supdoc's private copy, named by its hash in the args.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: yamllint_args,
        reader: Reader::Lint(yaml::lint_verdict),
    },
    ToolCartridge {
        name: "gofmt",
        language: Language::Go,
        binary: "gofmt",
        loads: Loads::Static,
        install: "install Go from https://go.dev/dl/",
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: GO_FRAGMENTS,
        holds: every_tag,
        vars: go::VARS,
        positions: go::positions,
        argv: gofmt_args,
        reader: Reader::Format(go::parse_error),
    },
    ToolCartridge {
        name: "go-vet",
        language: Language::Go,
        binary: "go",
        loads: Loads::Static,
        install: "install Go from https://go.dev/dl/",
        // go vet reads only the module supdoc writes for the block (see [`go::VetModule`]).
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: GO_VET_FRAGMENTS,
        holds: every_tag,
        vars: go::VET_VARS,
        positions: own_positions,
        argv: go_vet_args,
        reader: Reader::Lint(go::vet_verdict),
    },
    ToolCartridge {
        name: "pylint",
        language: Language::Python,
        binary: "pylint",
        loads: Loads::Project {
            held: Held {
                needs_trust: "pylint runs only in the project tier, which needs `supdoc trust`",
                trust_off: "pylint runs only in the project tier, and `SUPDOC_TRUST=0` turns \
                            trust off for this run",
                short: "pylint needs trust",
                didnt_run: "pylint didn't run on Python blocks: it imports their modules",
                off: "`lint = \"off\"` in `[features.code-blocks.python.pylint]`",
            },
            // It reads the project's config and imports its modules as its command line would.
            cwd: Cwd::Root,
            bins: &fixed::VENV_BINS,
        },
        install: "pipx install 'pylint==4.1.3'",
        // Never cached (see [`ToolCartridge::cacheable`]), so no config file keys its results.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: pylint_args,
        reader: Reader::Lint(pylint::lint_verdict),
    },
    ToolCartridge {
        name: "prettier",
        language: Language::TypeScript,
        binary: "prettier",
        loads: Loads::Project {
            held: Held {
                // Each language's reasons are its own, so a held step leads back to its row.
                needs_trust: "prettier runs on TypeScript blocks only in the project tier, which \
                              needs `supdoc trust`",
                trust_off: "prettier runs on TypeScript blocks only in the project tier, and \
                            `SUPDOC_TRUST=0` turns trust off for this run",
                short: "prettier needs trust",
                didnt_run: "prettier didn't run on TypeScript blocks: its config and plugins are \
                            the project's code",
                off: "`format = \"off\"` in `[features.code-blocks.typescript.prettier]`",
            },
            // It reads the project's config and loads its plugins as its command line would.
            cwd: Cwd::Root,
            bins: &fixed::NODE_BINS,
        },
        install: "npm install --global prettier@3.9.10",
        // Never cached (see [`ToolCartridge::cacheable`]), so no config file keys its results.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: prettier_args,
        reader: Reader::Format(prettier::parse_error),
    },
    ToolCartridge {
        name: "prettier",
        language: Language::JavaScript,
        binary: "prettier",
        loads: Loads::Project {
            held: Held {
                // Each language's reasons are its own, so a held step leads back to its row.
                needs_trust: "prettier runs on JavaScript blocks only in the project tier, which \
                              needs `supdoc trust`",
                trust_off: "prettier runs on JavaScript blocks only in the project tier, and \
                            `SUPDOC_TRUST=0` turns trust off for this run",
                short: "prettier needs trust",
                didnt_run: "prettier didn't run on JavaScript blocks: its config and plugins are \
                            the project's code",
                off: "`format = \"off\"` in `[features.code-blocks.javascript.prettier]`",
            },
            // It reads the project's config and loads its plugins as its command line would.
            cwd: Cwd::Root,
            bins: &fixed::NODE_BINS,
        },
        install: "npm install --global prettier@3.9.10",
        // Never cached (see [`ToolCartridge::cacheable`]), so no config file keys its results.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: prettier_args,
        reader: Reader::Format(prettier::parse_error),
    },
    ToolCartridge {
        name: "prettier",
        language: Language::Json,
        binary: "prettier",
        loads: Loads::Project {
            held: Held {
                // Each language's reasons are its own, so a held step leads back to its row.
                needs_trust: "prettier runs on JSON blocks only in the project tier, which \
                              needs `supdoc trust`",
                trust_off: "prettier runs on JSON blocks only in the project tier, and \
                            `SUPDOC_TRUST=0` turns trust off for this run",
                short: "prettier needs trust",
                didnt_run: "prettier didn't run on JSON blocks: its config and plugins are \
                            the project's code",
                off: "`format = \"off\"` in `[features.code-blocks.json.prettier]`",
            },
            // It reads the project's config and loads its plugins as its command line would.
            cwd: Cwd::Root,
            bins: &fixed::NODE_BINS,
        },
        install: "npm install --global prettier@3.9.10",
        // Never cached (see [`ToolCartridge::cacheable`]), so no config file keys its results.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: prettier_json_args,
        reader: Reader::Format(prettier::parse_error),
    },
    ToolCartridge {
        name: "prettier",
        language: Language::Yaml,
        binary: "prettier",
        loads: Loads::Project {
            held: Held {
                // Each language's reasons are its own, so a held step leads back to its row.
                needs_trust: "prettier runs on YAML blocks only in the project tier, which \
                              needs `supdoc trust`",
                trust_off: "prettier runs on YAML blocks only in the project tier, and \
                            `SUPDOC_TRUST=0` turns trust off for this run",
                short: "prettier needs trust",
                didnt_run: "prettier didn't run on YAML blocks: its config and plugins are \
                            the project's code",
                off: "`format = \"off\"` in `[features.code-blocks.yaml.prettier]`",
            },
            // It reads the project's config and loads its plugins as its command line would.
            cwd: Cwd::Root,
            bins: &fixed::NODE_BINS,
        },
        install: "npm install --global prettier@3.9.10",
        // Never cached (see [`ToolCartridge::cacheable`]), so no config file keys its results.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: prettier_yaml_args,
        reader: Reader::Format(prettier::parse_error),
    },
    ToolCartridge {
        name: "prettier",
        language: Language::Markdown,
        binary: "prettier",
        loads: Loads::Project {
            held: Held {
                // Each language's reasons are its own, so a held step leads back to its row.
                needs_trust: "prettier runs on Markdown blocks only in the project tier, which \
                              needs `supdoc trust`",
                trust_off: "prettier runs on Markdown blocks only in the project tier, and \
                            `SUPDOC_TRUST=0` turns trust off for this run",
                short: "prettier needs trust",
                didnt_run: "prettier didn't run on Markdown blocks: its config and plugins are \
                            the project's code",
                off: "`format = \"off\"` in `[features.code-blocks.markdown.prettier]`",
            },
            // It reads the project's config and loads its plugins as its command line would.
            cwd: Cwd::Root,
            bins: &fixed::NODE_BINS,
        },
        install: "npm install --global prettier@3.9.10",
        // Never cached (see [`ToolCartridge::cacheable`]), so no config file keys its results.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: prettier_markdown_args,
        reader: Reader::Format(prettier::parse_error),
    },
    ToolCartridge {
        name: "eslint",
        language: Language::TypeScript,
        binary: "eslint",
        loads: Loads::Project {
            held: Held {
                // Each language's reasons are its own, so a held step leads back to its row.
                needs_trust: "eslint runs on TypeScript blocks only in the project tier, which needs \
                              `supdoc trust`",
                trust_off: "eslint runs on TypeScript blocks only in the project tier, and \
                            `SUPDOC_TRUST=0` turns trust off for this run",
                short: "eslint needs trust",
                didnt_run: "eslint didn't run on TypeScript blocks: its config and plugins are the \
                            project's code",
                off: "`lint = \"off\"` in `[features.code-blocks.typescript.eslint]`",
            },
            // It reads the project's config and loads its plugins as its command line would.
            cwd: Cwd::Root,
            bins: &fixed::NODE_BINS,
        },
        install: "npm install --global eslint@10.14.0",
        // Never cached (see [`ToolCartridge::cacheable`]), so no config file keys its results.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: eslint_args,
        reader: Reader::Lint(eslint::lint_verdict),
    },
    ToolCartridge {
        name: "eslint",
        language: Language::JavaScript,
        binary: "eslint",
        loads: Loads::Project {
            held: Held {
                // Each language's reasons are its own, so a held step leads back to its row.
                needs_trust: "eslint runs on JavaScript blocks only in the project tier, which needs \
                              `supdoc trust`",
                trust_off: "eslint runs on JavaScript blocks only in the project tier, and \
                            `SUPDOC_TRUST=0` turns trust off for this run",
                short: "eslint needs trust",
                didnt_run: "eslint didn't run on JavaScript blocks: its config and plugins are the \
                            project's code",
                off: "`lint = \"off\"` in `[features.code-blocks.javascript.eslint]`",
            },
            // It reads the project's config and loads its plugins as its command line would.
            cwd: Cwd::Root,
            bins: &fixed::NODE_BINS,
        },
        install: "npm install --global eslint@10.14.0",
        // Never cached (see [`ToolCartridge::cacheable`]), so no config file keys its results.
        config_files: &[],
        modes: FULL_OR_OFF,
        fragments: &[],
        holds: every_tag,
        vars: &[],
        positions: own_positions,
        argv: eslint_args,
        reader: Reader::Lint(eslint::lint_verdict),
    },
];

/// `language`'s default formatter: its first format cartridge, whatever its tier.
#[must_use]
pub(crate) fn formatter(language: Language) -> Option<&'static ToolCartridge> {
    default_for(language, Step::Format)
}

/// `language`'s first cartridge for `step`: its default for the step.
#[must_use]
pub(crate) fn default_for(language: Language, step: Step) -> Option<&'static ToolCartridge> {
    CARTRIDGES
        .iter()
        .find(|tool| tool.language == language && tool.step() == step)
}

/// The tool `code_blocks` selects for `language`'s `step`: the custom command its `formatter`
/// or `linter` writes, the cartridge it names, else the default. A written command is never
/// replaced by the default. `None` when the language has no tool for the step.
#[must_use]
pub(crate) fn selected(code_blocks: &CodeBlocks, language: Language, step: Step) -> Option<Tool> {
    match code_blocks.choice(language, step) {
        Some(Choice::Command(command)) => Some(Tool::Custom(Arc::new(CustomTool::new(
            command.clone(),
            language,
            step,
        )))),
        Some(Choice::Known(name)) => named(language, step, name).map(Tool::Cartridge),
        None => default_for(language, step).map(Tool::Cartridge),
    }
}

/// The cartridge named `name` for `language`'s `step`, as config selects it.
#[must_use]
pub(crate) fn named(language: Language, step: Step, name: &str) -> Option<&'static ToolCartridge> {
    CARTRIDGES
        .iter()
        .find(|tool| tool.language == language && tool.step() == step && tool.name == name)
}

/// The `index`th block (1-based) of `doc`, the doc's absolute path, as a tool sees it:
/// `<doc dir>/<doc file name>.<index>.<extension>`, so the tool finds its config walking up from
/// the doc's real directory while the block itself arrives on stdin.
pub(crate) fn virtual_path(doc: &Path, index: usize, extension: &str) -> OsString {
    Suffix::new(&format!(".{index}.{extension}"))
        .and_then(|suffix| path::with_suffix(doc, &suffix))
        .map_or_else(|| doc.as_os_str().to_owned(), PathBuf::into_os_string)
}

/// `rustfmt`'s arguments: `--edition <year> --emit stdout`, then the project's `--config-path`.
/// The block arrives on stdin as written; rustfmt takes no stdin file name, so the doc and the
/// block's index don't appear.
fn rustfmt_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    ["--edition", invocation.edition.year(), "--emit", "stdout"]
        .into_iter()
        .map(OsString::from)
        .chain(invocation.project.iter().cloned())
        .collect()
}

/// `shfmt`'s arguments: `--filename <doc>.<index>.sh` (see [`virtual_path`]), then `-ln
/// <dialect>`.
fn shfmt_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    vec![
        OsString::from("--filename"),
        virtual_path(invocation.doc, invocation.index, "sh"),
        OsString::from("-ln"),
        OsString::from(invocation.dialect.language_name()),
    ]
}

/// `gofmt`'s arguments: none, so it reads the block on stdin and prints it formatted. gofmt has
/// no config file and no stdin file name, so neither the doc nor the project reaches it.
fn gofmt_args(_invocation: &Invocation<'_>) -> Vec<OsString> {
    Vec::new()
}

/// `go vet`'s arguments: `vet -json .`, the package in the module supdoc writes for the block,
/// where it runs (see [`go::VetModule`]). The doc and the project never reach it.
fn go_vet_args(_invocation: &Invocation<'_>) -> Vec<OsString> {
    ["vet", "-json", "."]
        .into_iter()
        .map(OsString::from)
        .collect()
}

/// `ruff format`'s arguments: the block on stdin under its virtual path, no cache.
fn ruff_format_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    vec![
        OsString::from("format"),
        OsString::from("--no-cache"),
        OsString::from("--stdin-filename"),
        virtual_path(invocation.doc, invocation.index, "py"),
        OsString::from("-"),
    ]
}

/// `black`'s arguments: quiet, the block on stdin under its virtual path.
fn black_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    vec![
        OsString::from("--quiet"),
        OsString::from("--stdin-filename"),
        virtual_path(invocation.doc, invocation.index, "py"),
        OsString::from("-"),
    ]
}

/// The rules `ruff check` leaves off for a block, which a snippet breaks for lack of context:
/// undefined names, unused imports and unused variables.
const RUFF_RELAXED: &str = "F821,F401,F841";

/// The rules pylint leaves off for a block, which a snippet breaks for lack of context: unused
/// imports and variables, a module docstring (a block is never a module) and undefined names.
const PYLINT_RELAXED: &str = "W0611,W0612,C0114,E0602";

/// pylint's arguments: the block on stdin under its virtual path, the `json2` report, no stats
/// saved under `HOME` (`--persistent=n`), and the snippet relaxations disabled.
fn pylint_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    vec![
        OsString::from("--from-stdin"),
        virtual_path(invocation.doc, invocation.index, "py"),
        OsString::from("--output-format=json2"),
        OsString::from("--persistent=n"),
        OsString::from(format!("--disable={PYLINT_RELAXED}")),
    ]
}

/// prettier's arguments on a TypeScript or JavaScript block: [`prettier_with`] the extension
/// from the tag as biome's (see [`biome::extension`]) and the parser from that extension (see
/// [`prettier::parser`]).
fn prettier_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    let extension = biome::extension(invocation.tag);
    prettier_with(invocation, extension, prettier::parser(extension))
}

/// prettier's arguments on a JSON block: [`prettier_with`] the `jsonc` parser for a block tagged
/// `jsonc`, else `json`, each under the extension the tag names (see [`biome::extension`]).
fn prettier_json_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    let extension = biome::extension(invocation.tag);
    let parser = if extension == "jsonc" {
        "jsonc"
    } else {
        "json"
    };
    prettier_with(invocation, extension, parser)
}

/// prettier's arguments on a YAML block: [`prettier_with`] the `yaml` parser, whichever tag.
fn prettier_yaml_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    prettier_with(invocation, "yaml", "yaml")
}

/// prettier's arguments on a Markdown block: [`prettier_with`] the `markdown` parser, whichever
/// tag.
fn prettier_markdown_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    prettier_with(invocation, "md", "markdown")
}

/// prettier's arguments, each overriding what the project's config could set to make it print the
/// block back unformatted (see [`prettier`]): no ignore file read, a doc under `node_modules`
/// formatted like any other, no pragma required, honored or inserted (a block is a snippet, never
/// a file that takes one), the whole block as the range, and `parser`, the one the tag names,
/// never one an `overrides` entry picks. Then the block on stdin under its virtual path with
/// `extension`. Each flag is one prettier [`prettier::FLOOR`] accepts.
fn prettier_with(invocation: &Invocation<'_>, extension: &str, parser: &str) -> Vec<OsString> {
    vec![
        OsString::from("--ignore-path="),
        OsString::from("--with-node-modules"),
        OsString::from("--no-require-pragma"),
        OsString::from("--no-check-ignore-pragma"),
        OsString::from("--no-insert-pragma"),
        OsString::from("--range-start=0"),
        OsString::from("--range-end=Infinity"),
        OsString::from(format!("--parser={parser}")),
        OsString::from("--stdin-filepath"),
        virtual_path(invocation.doc, invocation.index, extension),
    ]
}

/// eslint's arguments: the block on stdin under its virtual path, its extension from the tag as
/// biome's (see [`biome::extension`]), so the project's config objects match it by their
/// `files`, and the `json` report (see [`eslint`]).
fn eslint_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    vec![
        OsString::from("--stdin"),
        OsString::from("--stdin-filename"),
        virtual_path(
            invocation.doc,
            invocation.index,
            biome::extension(invocation.tag),
        ),
        OsString::from("--format"),
        OsString::from("json"),
    ]
}

/// `ruff check`'s arguments: the block on stdin under its virtual path, JSON diagnostics, no
/// cache, the snippet relaxations and the block's fragment relaxations ignored, and never
/// `--fix`.
fn ruff_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    let ignored = std::iter::once(RUFF_RELAXED)
        .chain(relaxed(RUFF_FRAGMENTS, invocation.fragment).iter().copied())
        .collect::<Vec<_>>()
        .join(",");
    vec![
        OsString::from("check"),
        OsString::from("--no-cache"),
        OsString::from("--output-format=json"),
        OsString::from("--stdin-filename"),
        virtual_path(invocation.doc, invocation.index, "py"),
        OsString::from("--ignore"),
        OsString::from(ignored),
        OsString::from("-"),
    ]
}

/// The checks `shellcheck` leaves off for a block, which a snippet breaks for lack of context:
/// a variable used but never assigned (SC2154), one assigned but never used (SC2034), and a
/// sourced file it can't follow (SC1091).
const SHELLCHECK_RELAXED: &str = "SC2154,SC2034,SC1091";

/// `shellcheck`'s arguments: `-s <shell>` for the dialect (POSIX is `sh`, mksh is `ksh`), no rc
/// file, json1 diagnostics, the snippet relaxations excluded, and the block on stdin. `-x` and
/// `--external-sources` are never passed (see [`shell`]).
fn shellcheck_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    let shell = match invocation.dialect {
        Dialect::Bash => "bash",
        Dialect::Posix => "sh",
        Dialect::Mksh => "ksh",
    };
    vec![
        OsString::from("-s"),
        OsString::from(shell),
        OsString::from("--norc"),
        OsString::from("--format=json1"),
        OsString::from(format!("--exclude={SHELLCHECK_RELAXED}")),
        OsString::from("-"),
    ]
}

/// `biome format`'s arguments: the block on stdin under its virtual path, whose extension the
/// block's tag picks (see [`biome::extension`]), no colors, then the project's `--config-path`.
fn biome_format_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    let mut stdin_path = OsString::from("--stdin-file-path=");
    stdin_path.push(virtual_path(
        invocation.doc,
        invocation.index,
        biome::extension(invocation.tag),
    ));
    ["format", "--colors=off"]
        .into_iter()
        .map(OsString::from)
        .chain([stdin_path])
        .chain(invocation.project.iter().cloned())
        .collect()
}

/// The rules `biome lint` leaves off for a block, which a snippet breaks for lack of context: the
/// unused variables, imports and function parameters a doc shows without the code that uses them.
const BIOME_RELAXED: [&str; 3] = [
    "correctness/noUnusedVariables",
    "correctness/noUnusedImports",
    "correctness/noUnusedFunctionParameters",
];

/// `biome lint`'s arguments: no colors, JSON diagnostics with none held back, a block biome's
/// config doesn't include passing rather than failing, the snippet relaxations skipped, the
/// project's `--config-path`, then the block's file. biome lint reports nothing for a block on
/// stdin, so the block is the private file `emit` names; without one it is named by its virtual
/// path, whose extension the block's tag picks (see [`biome::extension`]).
fn biome_lint_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    let file = invocation.emit.map_or_else(
        || {
            virtual_path(
                invocation.doc,
                invocation.index,
                biome::extension(invocation.tag),
            )
        },
        |file| file.as_os_str().to_owned(),
    );
    [
        "lint",
        "--colors=off",
        "--reporter=json",
        "--max-diagnostics=none",
        "--no-errors-on-unmatched",
    ]
    .into_iter()
    .map(OsString::from)
    .chain(
        BIOME_RELAXED
            .iter()
            .map(|rule| OsString::from(format!("--skip={rule}"))),
    )
    .chain(invocation.project.iter().cloned())
    .chain([file])
    .collect()
}

/// The rules rumdl leaves off for a block: MD041, which wants a file to open with a heading, and
/// MD051 and MD057, which check a block's links against the doc's real directory and the files
/// beside it, which the block's text is not. Line length (MD013) is left to the project's config.
const RUMDL_RELAXED: &str = "--extend-disable=MD041,MD051,MD057";

/// The name rumdl is told its stdin has: a doc that is itself Markdown gives its own path, so a
/// per-file entry naming the doc matches its blocks; any other doc, such as a Rust source file,
/// gives `<doc>.<n>.md` (see [`virtual_path`]).
fn rumdl_stdin_name(invocation: &Invocation<'_>) -> OsString {
    if invocation
        .doc
        .extension()
        .is_some_and(|extension| extension == "md")
    {
        invocation.doc.as_os_str().to_owned()
    } else {
        virtual_path(invocation.doc, invocation.index, "md")
    }
}

/// `rumdl fmt`'s arguments: no cache, no `exclude` (a project's would skip the block and pass it),
/// the snippet relaxation, the project's `--config` or `--no-config`, then the block on stdin under its virtual path.
fn rumdl_format_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    [
        OsString::from("fmt"),
        OsString::from("--no-cache"),
        OsString::from("--no-exclude"),
    ]
    .into_iter()
    .chain([OsString::from(RUMDL_RELAXED)])
    .chain(invocation.project.iter().cloned())
    .chain([
        OsString::from("--stdin-filename"),
        rumdl_stdin_name(invocation),
        OsString::from("-"),
    ])
    .collect()
}

/// `rumdl check`'s arguments: no cache, no `exclude`, JSON diagnostics, the snippet relaxation, the project's
/// `--config` or `--no-config`, then the block on stdin under its virtual path. Never `--fix`.
fn rumdl_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    [
        OsString::from("check"),
        OsString::from("--no-cache"),
        OsString::from("--no-exclude"),
        OsString::from("--output-format=json"),
        OsString::from(RUMDL_RELAXED),
    ]
    .into_iter()
    .chain(invocation.project.iter().cloned())
    .chain([
        OsString::from("--stdin"),
        OsString::from("--stdin-filename"),
        rumdl_stdin_name(invocation),
    ])
    .collect()
}

/// `yamlfmt`'s arguments: the block on stdin (`-in`), no config from the system config directory,
/// then the project's `-conf <private copy>` (see [`yaml::project_args`]). yamlfmt takes no stdin
/// file name, so the doc and the block's index don't appear.
fn yamlfmt_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    [OsString::from("-in"), OsString::from("-no_global_conf")]
        .into_iter()
        .chain(invocation.project.iter().cloned())
        .collect()
}

/// `yamllint`'s arguments: diagnostics one per line, a warning failing the run as an error does,
/// the project's `-c <private copy>` (see [`yaml::lint_args`]), with the snippet relaxation in
/// it, then the block on stdin. yamllint takes no stdin file name, so the doc and the block's
/// index don't appear.
fn yamllint_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    [
        OsString::from("-f"),
        OsString::from("parsable"),
        OsString::from("-s"),
    ]
    .into_iter()
    .chain(invocation.project.iter().cloned())
    .chain([OsString::from("-")])
    .collect()
}

/// `tombi format`'s arguments: offline, so no schema is fetched, quiet, then the block on stdin
/// under its virtual path. tombi gets its config through `XDG_CONFIG_HOME` (see [`tombi`]), not
/// a flag.
fn tombi_format_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    vec![
        OsString::from("format"),
        OsString::from("--offline"),
        OsString::from("--quiet"),
        OsString::from("--stdin-filename"),
        virtual_path(invocation.doc, invocation.index, "toml"),
        OsString::from("-"),
    ]
}

/// `tombi lint`'s arguments: offline, JSON diagnostics on stdout (its summary stays on stderr,
/// which tells a run that linted nothing, see [`tombi::linted_nothing`]), then the block on stdin
/// under its virtual path.
fn tombi_lint_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    vec![
        OsString::from("lint"),
        OsString::from("--offline"),
        OsString::from("--diagnostics-format"),
        OsString::from("json"),
        OsString::from("--diagnostics-file"),
        OsString::from("/dev/stdout"),
        OsString::from("--stdin-filename"),
        virtual_path(invocation.doc, invocation.index, "toml"),
        OsString::from("-"),
    ]
}

/// `dockerfmt`'s arguments: a four-space indent and one final newline, with no file, so it reads
/// the block on stdin and no `.editorconfig`. Without `-n` it drops the block's final newline.
fn dockerfmt_args(_invocation: &Invocation<'_>) -> Vec<OsString> {
    ["-i", "4", "-n"].map(OsString::from).to_vec()
}

/// `superhtml`'s arguments: format the block on stdin, printing it on stdout, with element and
/// attribute validation off (`--syntax-only`). Validation changes neither the output nor the exit
/// status, only what goes to stderr, so with it off every error superhtml prints on a failed run
/// is one that stopped formatting, and the first one names the line (see
/// [`superhtml::parse_error`]).
fn superhtml_args(_invocation: &Invocation<'_>) -> Vec<OsString> {
    ["fmt", "--stdin", "--syntax-only"]
        .map(OsString::from)
        .to_vec()
}

/// `mbake`'s arguments: `format --stdin`, the block on stdin and the formatted Makefile on
/// stdout, then the private config the project contributes (`--config <file>`, see
/// [`mbake::project_args`]), so mbake searches for no config. Never `--version`, which asks the
/// package index for a newer release, and never `validate` or `--validate`, which run `make`.
fn mbake_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    let mut args = vec![OsString::from("format"), OsString::from("--stdin")];
    args.extend(invocation.project.iter().cloned());
    args
}

/// `hadolint`'s arguments: JSON on stdout with no colour, every finding's level counted as
/// failing, the private config the project contributes (`--config <file>`, see
/// [`hadolint::project_args`]), then `-` for the block on stdin. supdoc reads the JSON, never the
/// exit status (see [`hadolint::lint_verdict`]).
fn hadolint_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        OsString::from("--no-color"),
        OsString::from("-f"),
        OsString::from("json"),
        OsString::from("--failure-threshold=style"),
    ];
    args.extend(invocation.project.iter().cloned());
    args.push(OsString::from("-"));
    args
}

/// `sqruff fix`'s arguments: the block on stdin (`-`), the fixed SQL on stdout, no config but
/// the one supdoc names (`--ignore-local-config`), then the private config and the block's
/// dialect the project contributes (`--config <file> --dialect <name>`, see
/// [`sqruff::project_args`]). The private config keeps only the layout and capitalisation rules,
/// so `fix` changes nothing else; the format step compares the output with the block, never the
/// exit status (see [`super::format::judge`]).
fn sqruff_format_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["fix", "-", "--ignore-local-config"]
        .map(OsString::from)
        .to_vec();
    args.extend(invocation.project.iter().cloned());
    args
}

/// The lints clippy leaves off for a block, which a snippet breaks for lack of context, as
/// `-A` flags after the project's own so they win.
const CLIPPY_RELAXED: [&str; 4] = [
    "unused",
    "dead_code",
    "missing_docs",
    "clippy::missing_docs_in_private_items",
];

/// `clippy-driver`'s arguments: `--edition <E> --crate-type lib --emit=metadata=<emit>
/// --error-format=json`, the project build's `--extern` and `-L` flags when the block is linked
/// against it, the project's `lints` flags, the snippet relaxations, and `-` for the block on
/// stdin. Nothing else from the project: no `--cfg`.
fn clippy_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    let mut metadata = OsString::from("--emit=metadata");
    if let Some(emit) = invocation.emit {
        metadata.push("=");
        metadata.push(emit);
    }
    let mut args: Vec<OsString> = vec![
        "--edition".into(),
        invocation.edition.year().into(),
        "--crate-type".into(),
        "lib".into(),
        metadata,
        "--error-format=json".into(),
    ];
    args.extend(invocation.externs.iter().cloned());
    args.extend(invocation.project.iter().cloned());
    for lint in CLIPPY_RELAXED {
        args.extend(["-A".into(), lint.into()]);
    }
    args.push("-".into());
    args
}

/// Where a tool's binary was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Found {
    /// On `PATH`, outside the project root, at the real path held: the path a tool is run by.
    OnPath(PathBuf),
    /// Chosen by the checkout, at the real path held: in one of the root's own bin directories,
    /// wherever it resolves, or on `PATH` at a real path inside the root. Only a
    /// [`Tier::Project`] tool runs one.
    InsideRoot(PathBuf),
    /// In none of the root's bin directories and not on `PATH`.
    Missing,
}

/// Looks `binary` up on `env`'s `PATH`, judging the first match's real path against `root`.
///
/// Effect: reads file metadata along `PATH` and resolves symlinks.
#[must_use]
pub(crate) fn find(binary: &str, env: &ToolEnv, root: &Path) -> Found {
    find_in(binary, &[], env, root)
}

/// Looks `binary` up in `bins`, directories relative to `root`, in order, then as [`find`] does.
/// A match in `bins` is the checkout's choice wherever it resolves.
///
/// Effect: reads file metadata in `bins` and along `PATH`, and resolves symlinks.
#[must_use]
pub(crate) fn find_in(binary: &str, bins: &[FixedPath], env: &ToolEnv, root: &Path) -> Found {
    lookup(binary, bins, env.path(), env.pathext(), root)
}

/// [`find_in`] over an explicit `path` and `pathext`.
///
/// Effect: reads file metadata in `bins` and along `path`, and resolves symlinks.
fn lookup(
    binary: &str,
    bins: &[FixedPath],
    path: Option<&OsStr>,
    pathext: Option<&OsStr>,
    root: &Path,
) -> Found {
    let local = bins
        .iter()
        .map(|dir| path::beneath(root, dir))
        .find_map(|dir| {
            candidate_names(binary, pathext)
                .iter()
                .filter_map(|name| in_dir(&dir, name))
                .find(|candidate| is_executable_file(candidate))
                .and_then(|candidate| candidate.canonicalize().ok())
        });
    if let Some(real) = local {
        return Found::InsideRoot(real);
    }
    let Some(real) = path.and_then(|path| first_on_path(binary, path, pathext)) else {
        return Found::Missing;
    };
    if inside(&real, root) {
        Found::InsideRoot(real)
    } else {
        Found::OnPath(real)
    }
}

/// The `PATH` entries a lookup searches, in order: the absolute ones. An empty entry (the
/// current directory to a shell) and every relative one are skipped.
fn search_dirs(path: &OsStr) -> impl Iterator<Item = PathBuf> {
    std::env::split_paths(path).filter(|dir| dir.is_absolute())
}

/// The file names a lookup tries for `binary`, in order. Without `pathext` (every OS but
/// Windows) that is `binary` alone. With it, only entries of a dot and one or more ASCII letters
/// or digits count; any other entry (empty, `.`, or holding a separator, `:` or NUL) is skipped.
/// Of those, a `binary` already ending in one of its extensions
/// (compared without case) is tried as it is; any other gets each extension appended, in
/// `pathext` order, so `ruff` finds `ruff.exe` and a `.cmd` shim only when `.CMD` is listed.
fn candidate_names(binary: &str, pathext: Option<&OsStr>) -> Vec<OsString> {
    let Some(pathext) = pathext else {
        return vec![binary.into()];
    };
    let pathext = pathext.to_string_lossy();
    let extensions: Vec<&str> = pathext
        .split(';')
        .filter_map(|extension| extension.strip_prefix('.'))
        .filter(|extension| {
            !extension.is_empty() && extension.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .collect();
    let listed = Path::new(binary).extension().is_some_and(|own| {
        let own = own.to_string_lossy();
        extensions
            .iter()
            .any(|extension| extension.eq_ignore_ascii_case(&own))
    });
    if listed {
        return vec![binary.into()];
    }
    extensions
        .into_iter()
        .map(|extension| OsString::from(format!("{binary}.{extension}")))
        .collect()
}

/// `dir` joined with the candidate `name`, or `None` when `name` would leave `dir` on this
/// platform.
fn in_dir(dir: &Path, name: &OsStr) -> Option<PathBuf> {
    let name = NativeName::new(name.to_str()?)?;
    Some(path::within(dir, &name))
}

/// The real path of the first executable regular file named `binary` (or, with `pathext`, one of
/// its [`candidate_names`]) in [`search_dirs`].
///
/// Effect: reads file metadata and resolves symlinks.
fn first_on_path(binary: &str, path: &OsStr, pathext: Option<&OsStr>) -> Option<PathBuf> {
    let names = candidate_names(binary, pathext);
    search_dirs(path)
        .flat_map(|dir| names.iter().filter_map(move |name| in_dir(&dir, name)))
        .filter(|candidate| is_executable_file(candidate))
        .find_map(|candidate| candidate.canonicalize().ok())
}

/// Whether `real`, a resolved path, is inside `root`: under the root's real path by whole
/// components, or under a directory that is the root's own file however it is spelled.
///
/// Effect: reads the metadata of `root` and of each ancestor of `real`.
pub(super) fn inside(real: &Path, root: &Path) -> bool {
    Boundary::resolve(root).contains(real) || under_same_file(real, root)
}

/// Whether some ancestor of `real` has the same device and inode as `root`, so a firmlink or
/// bind-mount spelling of the root counts as the root.
///
/// Effect: reads the metadata of `root` and of each ancestor of `real`.
#[cfg(unix)]
fn under_same_file(real: &Path, root: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(root) = std::fs::metadata(root) else {
        return false;
    };
    real.ancestors()
        .filter_map(|ancestor| std::fs::metadata(ancestor).ok())
        .any(|meta| meta.dev() == root.dev() && meta.ino() == root.ino())
}

/// Whether some ancestor of `real` is `root` by file identity: never known off Unix.
#[cfg(not(unix))]
fn under_same_file(_real: &Path, _root: &Path) -> bool {
    false
}

/// Whether `path`, following symlinks, is a regular file someone may execute.
///
/// Effect: reads the file's metadata.
pub(super) fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && executable(&meta))
}

/// Whether `meta`'s permissions let someone execute the file.
#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

/// Whether `meta`'s permissions let someone execute the file: any regular file off Unix.
#[cfg(not(unix))]
fn executable(_meta: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use crate::cli::testing::symlink;
    use std::ffi::{OsStr, OsString};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use super::{
        CARTRIDGES, Cwd, Found, Hold, Invocation, Tier, ToolCartridge, ToolEnv, find, hold,
        search_dirs,
    };
    #[cfg(unix)]
    use super::{find_in, under_same_file};
    use crate::code_blocks::fragment::{Frame, Kind};
    use crate::config::code_blocks::{Dialect, Edition, Language};
    use crate::path::{FixedPath, fixed};

    /// The cartridge named `name`.
    fn cartridge(name: &str) -> &'static ToolCartridge {
        CARTRIDGES
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("the cartridges should include {name}"))
    }

    /// The virtual path `file` has beside `/p/docs/a.md`, joined as the platform joins it: a tool
    /// is given a native path, so the separator before `file` is the platform's.
    fn docs(file: &str) -> String {
        Path::new("/p/docs")
            .join(file)
            .to_string_lossy()
            .into_owned()
    }

    /// `name`'s arguments for the `index`th block of `/p/docs/a.md`, a 2024 bash block, with
    /// `project`'s flags and no metadata path.
    fn args_of(name: &str, index: usize, project: &[OsString]) -> Vec<OsString> {
        cartridge(name).args(&Invocation {
            doc: Path::new("/p/docs/a.md"),
            tag: "",
            index,
            edition: Edition::E2024,
            dialect: Dialect::Bash,
            project,
            emit: None,
            externs: &[],
            fragment: None,
        })
    }

    #[test]
    fn a_bare_name_gets_each_pathext_extension_in_order() {
        let names = super::candidate_names("ruff", Some(OsStr::new(".COM;.EXE;.CMD")));
        assert_eq!(
            names,
            ["ruff.COM", "ruff.EXE", "ruff.CMD"].map(OsString::from)
        );
    }

    #[test]
    fn a_name_ending_in_a_listed_extension_is_tried_as_it_is_without_case() {
        let names = super::candidate_names("biome.cmd", Some(OsStr::new(".EXE;.CMD")));
        assert_eq!(names, [OsString::from("biome.cmd")]);
    }

    #[test]
    fn a_shim_extension_missing_from_pathext_is_never_tried() {
        let names = super::candidate_names("biome", Some(OsStr::new(".COM;.EXE")));
        assert!(!names.contains(&OsString::from("biome.cmd")), "{names:?}");
    }

    #[test]
    fn without_pathext_only_the_exact_name_is_tried() {
        assert_eq!(
            super::candidate_names("ruff", None),
            [OsString::from("ruff")]
        );
    }

    #[test]
    fn only_a_dot_and_ascii_letters_or_digits_is_a_pathext_entry() {
        let names = super::candidate_names(
            "tool",
            Some(OsStr::new(
                ";;.;..;.EXE\\..\\x;.a/b;.C:;.x\0y;.;.ok1;ZIP;.é;.CMD",
            )),
        );
        assert_eq!(names, ["tool.ok1", "tool.CMD"].map(OsString::from));
        let empties = super::candidate_names("tool", Some(OsStr::new(";;")));
        assert!(empties.is_empty(), "{empties:?}");
    }

    /// Creates an executable file `name` in `dir`.
    fn exe(dir: &Path, name: &str) {
        let file = dir.join(name);
        fs::write(&file, "").expect("the file should be written");
        #[cfg(unix)]
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755))
            .expect("the file should become executable");
    }

    #[test]
    fn a_path_directory_holding_only_tool_exe_is_found_for_tool_when_pathext_is_given() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).expect("bin dir");
        exe(&bin, "tool.EXE");
        let path = std::env::join_paths([bin.canonicalize().expect("bin path")]).expect("path");
        let with = super::first_on_path("tool", &path, Some(OsStr::new(".EXE")));
        assert!(with.is_some(), "found with PATHEXT");
        assert_eq!(super::first_on_path("tool", &path, None), None);
    }

    #[test]
    fn each_directory_tries_every_extension_before_the_next_directory() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (first, second) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir(&first).expect("a dir");
        fs::create_dir(&second).expect("b dir");
        exe(&first, "tool.CMD");
        exe(&second, "tool.EXE");
        let path = std::env::join_paths([
            first.canonicalize().expect("a path"),
            second.canonicalize().expect("b path"),
        ])
        .expect("path");
        let found = super::first_on_path("tool", &path, Some(OsStr::new(".EXE;.CMD")))
            .expect("a tool should be found");
        assert!(
            found.ends_with("a/tool.CMD") || found.ends_with("a\\tool.CMD"),
            "{found:?}"
        );
    }

    #[test]
    fn within_one_directory_extensions_are_tried_in_pathext_order() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bin = dir.path().canonicalize().expect("bin path");
        exe(&bin, "tool.CMD");
        exe(&bin, "tool.EXE");
        let path = std::env::join_paths([&bin]).expect("path");
        let found = super::first_on_path("tool", &path, Some(OsStr::new(".EXE;.CMD")))
            .expect("a tool should be found");
        assert_eq!(found.file_name(), Some(OsStr::new("tool.EXE")), "{found:?}");
    }

    #[test]
    fn a_name_that_would_leave_the_directory_is_not_looked_up() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).expect("bin dir");
        exe(dir.path(), "outside");
        let path = std::env::join_paths([bin.canonicalize().expect("bin path")]).expect("path");
        for name in ["../outside", "..", ".", "a/b"] {
            assert_eq!(super::first_on_path(name, &path, None), None, "{name:?}");
        }
        #[cfg(windows)]
        for name in ["a\\b", "C:x", "a:b"] {
            assert_eq!(super::first_on_path(name, &path, None), None, "{name:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn on_unix_a_name_with_a_colon_or_backslash_is_found_on_path() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bin = dir.path().canonicalize().expect("bin path");
        exe(&bin, "my:fmt");
        exe(&bin, "a\\b");
        let path = std::env::join_paths([&bin]).expect("path");
        for name in ["my:fmt", "a\\b"] {
            let found = super::first_on_path(name, &path, None)
                .unwrap_or_else(|| panic!("{name:?} should be found"));
            assert_eq!(found.file_name(), Some(OsStr::new(name)), "{found:?}");
        }
    }

    #[test]
    fn lookup_passes_pathext_to_the_path_search() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("root");
        let bin = dir.path().join("bin");
        fs::create_dir(&root).expect("root dir");
        fs::create_dir(&bin).expect("bin dir");
        exe(&bin, "tool.EXE");
        let path = std::env::join_paths([bin.canonicalize().expect("bin path")]).expect("path");
        let found = super::lookup("tool", &[], Some(&path), Some(OsStr::new(".EXE")), &root);
        assert!(matches!(found, Found::OnPath(_)), "{found:?}");
        let without = super::lookup("tool", &[], Some(&path), None, &root);
        assert_eq!(without, Found::Missing);
    }

    #[test]
    fn a_root_bin_directory_holding_only_tool_exe_is_the_checkouts_choice_when_pathext_is_given() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().canonicalize().expect("root path");
        let bin = root.join("node_modules/.bin");
        fs::create_dir_all(&bin).expect("bin dir");
        exe(&bin, "tool.EXE");
        let found = super::lookup(
            "tool",
            &fixed::NODE_BINS,
            None,
            Some(OsStr::new(".EXE")),
            &root,
        );
        assert!(matches!(found, Found::InsideRoot(_)), "{found:?}");
        let without = super::lookup("tool", &fixed::NODE_BINS, None, None, &root);
        assert_eq!(without, Found::Missing);
    }

    #[cfg(not(windows))]
    #[test]
    fn off_windows_the_tool_environment_keeps_only_the_five_unix_names() {
        let env = ToolEnv::from_vars(|_| Some(OsString::from("v")));
        let names: Vec<&str> = env.vars().map(|(name, _)| name).collect();
        assert_eq!(
            names,
            [
                "PATH",
                "HOME",
                "RUSTUP_HOME",
                "CARGO_HOME",
                "XDG_CACHE_HOME"
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_the_tool_environment_also_keeps_the_names_a_process_needs_to_start() {
        let env = ToolEnv::from_vars(|_| Some(OsString::from("v")));
        let names: Vec<&str> = env.vars().map(|(name, _)| name).collect();
        for kept in [
            "SystemRoot",
            "SystemDrive",
            "PATHEXT",
            "TEMP",
            "TMP",
            "COMSPEC",
            "LOCALAPPDATA",
            "USERPROFILE",
        ] {
            assert!(names.contains(&kept), "{kept} missing from {names:?}");
        }
    }

    #[test]
    fn each_cartridge_declares_its_language_step_binary_and_tier() {
        let declared: Vec<_> = CARTRIDGES
            .iter()
            .map(|tool| {
                format!(
                    "{} {:?} {:?} {} {:?}",
                    tool.name,
                    tool.language,
                    tool.step(),
                    tool.binary,
                    tool.tier()
                )
            })
            .collect();

        assert_eq!(
            declared,
            [
                "rustfmt Rust Format rustfmt Static",
                "shfmt Shell Format shfmt Static",
                "ruff-format Python Format ruff Static",
                "black Python Format black Static",
                "ruff Python Lint ruff Static",
                "shellcheck Shell Lint shellcheck Static",
                "clippy Rust Lint clippy-driver Project",
                "biome-format TypeScript Format biome Static",
                "biome-format JavaScript Format biome Static",
                "biome-format Json Format biome Static",
                "biome-format Css Format biome Static",
                "biome-lint TypeScript Lint biome Static",
                "biome-lint JavaScript Lint biome Static",
                "biome-lint Json Lint biome Static",
                "biome-lint Css Lint biome Static",
                "rumdl-format Markdown Format rumdl Static",
                "rumdl Markdown Lint rumdl Static",
                "tombi-format Toml Format tombi Static",
                "tombi-lint Toml Lint tombi Static",
                "dockerfmt Dockerfile Format dockerfmt Static",
                "hadolint Dockerfile Lint hadolint Static",
                "sqruff-format Sql Format sqruff Static",
                "mbake Makefile Format mbake Static",
                "superhtml Html Format superhtml Static",
                "yamlfmt Yaml Format yamlfmt Static",
                "yamllint Yaml Lint yamllint Static",
                "gofmt Go Format gofmt Static",
                "go-vet Go Lint go Static",
                "pylint Python Lint pylint Project",
                "prettier TypeScript Format prettier Project",
                "prettier JavaScript Format prettier Project",
                "prettier Json Format prettier Project",
                "prettier Yaml Format prettier Project",
                "prettier Markdown Format prettier Project",
                "eslint TypeScript Lint eslint Project",
                "eslint JavaScript Lint eslint Project",
            ]
        );
    }

    #[test]
    fn dockerfmt_reads_stdin_with_a_four_space_indent_keeping_the_final_newline() {
        assert_eq!(
            args_of("dockerfmt", 1, &[]),
            ["-i", "4", "-n"].map(OsString::from)
        );
    }

    #[test]
    fn mbake_formats_stdin_with_the_private_config_and_never_asks_for_its_version_or_validates() {
        let config = ["--config", "/c/bake.toml"].map(OsString::from);
        let args = args_of("mbake", 1, &config);

        assert_eq!(
            args,
            ["format", "--stdin", "--config", "/c/bake.toml"].map(OsString::from)
        );
        for banned in ["--version", "validate", "--validate", "update"] {
            assert!(
                !args.iter().any(|arg| arg == banned),
                "mbake must never be run with `{banned}`"
            );
        }
    }

    #[test]
    fn superhtml_formats_stdin_with_validation_off() {
        assert_eq!(
            args_of("superhtml", 1, &[]),
            ["fmt", "--stdin", "--syntax-only"].map(OsString::from)
        );
    }

    #[test]
    fn hadolint_reads_stdin_as_json_counting_every_level_with_the_private_config_before_the_dash() {
        let config = ["--config", "/c/hadolint.yaml"].map(OsString::from);
        assert_eq!(
            args_of("hadolint", 1, &config),
            [
                "--no-color",
                "-f",
                "json",
                "--failure-threshold=style",
                "--config",
                "/c/hadolint.yaml",
                "-",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn sqruff_fixes_stdin_ignoring_every_local_config_with_the_private_config_and_dialect_last() {
        let project = ["--config", "/c/sqruff.cfg", "--dialect", "tsql"].map(OsString::from);
        assert_eq!(
            args_of("sqruff-format", 1, &project),
            [
                "fix",
                "-",
                "--ignore-local-config",
                "--config",
                "/c/sqruff.cfg",
                "--dialect",
                "tsql",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn pylint_reads_the_block_from_stdin_under_its_virtual_path_as_json2_saving_no_stats() {
        assert_eq!(
            args_of("pylint", 2, &[]),
            [
                "--from-stdin",
                &*docs("a.md.2.py"),
                "--output-format=json2",
                "--persistent=n",
                "--disable=W0611,W0612,C0114,E0602",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn prettier_overrides_ignore_pragma_range_and_parser_and_gets_the_blocks_virtual_path() {
        for (tag, parser, path) in [
            ("ts", "--parser=typescript", "a.md.3.ts"),
            ("tsx", "--parser=typescript", "a.md.3.tsx"),
            ("javascript", "--parser=babel", "a.md.3.js"),
            ("mjs", "--parser=babel", "a.md.3.mjs"),
        ] {
            let args = cartridge("prettier").args(&Invocation {
                doc: Path::new("/p/docs/a.md"),
                tag,
                index: 3,
                edition: Edition::E2024,
                dialect: Dialect::Bash,
                project: &[],
                emit: None,
                externs: &[],
                fragment: None,
            });

            assert_eq!(
                args,
                [
                    "--ignore-path=",
                    "--with-node-modules",
                    "--no-require-pragma",
                    "--no-check-ignore-pragma",
                    "--no-insert-pragma",
                    "--range-start=0",
                    "--range-end=Infinity",
                    parser,
                    "--stdin-filepath",
                    &*docs(path)
                ]
                .map(OsString::from),
                "{tag}"
            );
        }
    }

    #[test]
    fn prettier_reads_json_yaml_and_markdown_blocks_by_the_parser_their_tag_names() {
        for (language, tag, parser, path) in [
            (Language::Json, "json", "--parser=json", "a.md.3.json"),
            (Language::Json, "jsonc", "--parser=jsonc", "a.md.3.jsonc"),
            (Language::Yaml, "yaml", "--parser=yaml", "a.md.3.yaml"),
            (Language::Yaml, "yml", "--parser=yaml", "a.md.3.yaml"),
            (Language::Markdown, "md", "--parser=markdown", "a.md.3.md"),
            (
                Language::Markdown,
                "markdown",
                "--parser=markdown",
                "a.md.3.md",
            ),
        ] {
            let tool = CARTRIDGES
                .iter()
                .find(|tool| tool.name == "prettier" && tool.language == language)
                .unwrap_or_else(|| panic!("the cartridges should include prettier for {tag}"));
            let args = tool.args(&Invocation {
                doc: Path::new("/p/docs/a.md"),
                tag,
                index: 3,
                edition: Edition::E2024,
                dialect: Dialect::Bash,
                project: &[],
                emit: None,
                externs: &[],
                fragment: None,
            });

            assert_eq!(
                args,
                [
                    "--ignore-path=",
                    "--with-node-modules",
                    "--no-require-pragma",
                    "--no-check-ignore-pragma",
                    "--no-insert-pragma",
                    "--range-start=0",
                    "--range-end=Infinity",
                    parser,
                    "--stdin-filepath",
                    &*docs(path)
                ]
                .map(OsString::from),
                "{tag}"
            );
        }
    }

    #[test]
    fn eslint_reads_the_block_from_stdin_under_its_virtual_path_and_reports_json() {
        for (tag, path) in [
            ("ts", "a.md.3.ts"),
            ("tsx", "a.md.3.tsx"),
            ("javascript", "a.md.3.js"),
            ("cjs", "a.md.3.cjs"),
        ] {
            let args = cartridge("eslint").args(&Invocation {
                doc: Path::new("/p/docs/a.md"),
                tag,
                index: 3,
                edition: Edition::E2024,
                dialect: Dialect::Bash,
                project: &[],
                emit: None,
                externs: &[],
                fragment: None,
            });

            assert_eq!(
                args,
                [
                    "--stdin",
                    "--stdin-filename",
                    &*docs(path),
                    "--format",
                    "json"
                ]
                .map(OsString::from),
                "{tag}"
            );
        }
    }

    #[test]
    fn each_project_tier_cartridge_names_itself_in_its_trust_words() {
        for tool in CARTRIDGES
            .iter()
            .filter(|tool| tool.tier() == Tier::Project)
        {
            let held = tool
                .held()
                .unwrap_or_else(|| panic!("{} is project-tier without trust words", tool.name));
            let named = [held.needs_trust, held.trust_off, held.didnt_run]
                .into_iter()
                .all(|words| words.starts_with(&format!("{} ", tool.name)));
            let off = format!(
                "`{} = \"off\"` in `[features.code-blocks.{}.{}]`",
                tool.step().key(),
                tool.language.table(),
                tool.name
            );

            assert!(
                named && held.short == format!("{} needs trust", tool.name) && held.off == off,
                "{} should name itself in each reason and its own off setting, got {held:?}",
                tool.name
            );
            assert_eq!(
                (hold(held.needs_trust), hold(held.trust_off)),
                (
                    Some(Hold {
                        held,
                        turned_off: false
                    }),
                    Some(Hold {
                        held,
                        turned_off: true
                    })
                ),
                "{}'s reasons should lead back to it",
                tool.name
            );
        }
    }

    #[test]
    fn every_cartridge_accepts_full_and_off() {
        for tool in &CARTRIDGES {
            assert_eq!(tool.modes, ["full", "off"], "{}", tool.name);
        }
    }

    #[test]
    fn a_formatter_reads_a_parse_error_and_a_linter_none() {
        let stderr = "error: expected one of `;`\n --> <stdin>:3:9\n";

        assert!(cartridge("rustfmt").parse_error(stderr).is_some());
        assert_eq!(cartridge("clippy").parse_error(stderr), None);
    }

    #[test]
    fn rustfmt_reads_the_block_as_its_edition_then_the_projects_config_path() {
        assert_eq!(
            args_of(
                "rustfmt",
                1,
                &["--config-path".into(), "/p/rustfmt.toml".into()]
            ),
            [
                "--edition",
                "2024",
                "--emit",
                "stdout",
                "--config-path",
                "/p/rustfmt.toml"
            ]
        );
    }

    #[test]
    fn shfmt_names_the_block_by_its_index_then_its_dialect() {
        let posix = cartridge("shfmt").args(&Invocation {
            doc: Path::new("/p/docs/a.md"),
            tag: "",
            index: 3,
            edition: Edition::E2024,
            dialect: Dialect::Posix,
            project: &[],
            emit: None,
            externs: &[],
            fragment: None,
        });

        assert_eq!(posix, ["--filename", &*docs("a.md.3.sh"), "-ln", "posix"]);
    }

    #[test]
    fn biome_lint_skips_the_unused_rules_and_names_the_private_file_or_else_the_virtual_path() {
        let project = [OsString::from("--config-path=/c/biome.json")];
        let lint = |emit: Option<&Path>| {
            cartridge("biome-lint").args(&Invocation {
                doc: Path::new("/p/docs/a.md"),
                tag: "tsx",
                index: 2,
                edition: Edition::E2024,
                dialect: Dialect::Bash,
                project: &project,
                emit,
                externs: &[],
                fragment: None,
            })
        };
        let flags = [
            "lint",
            "--colors=off",
            "--reporter=json",
            "--max-diagnostics=none",
            "--no-errors-on-unmatched",
            "--skip=correctness/noUnusedVariables",
            "--skip=correctness/noUnusedImports",
            "--skip=correctness/noUnusedFunctionParameters",
            "--config-path=/c/biome.json",
        ];

        let named = docs("a.md.2.tsx");

        assert_eq!(
            lint(None),
            flags
                .iter()
                .chain(&[named.as_str()])
                .map(OsString::from)
                .collect::<Vec<_>>(),
            "a cache key names the block by its virtual path"
        );
        assert_eq!(
            lint(Some(Path::new("/private/run/a.md.2.tsx"))),
            flags
                .iter()
                .chain(&["/private/run/a.md.2.tsx"])
                .map(OsString::from)
                .collect::<Vec<_>>(),
            "a run names the private file the block was written to"
        );
    }

    #[test]
    fn rumdl_reads_stdin_under_the_virtual_path_with_the_project_config_and_md041_off() {
        let project = [OsString::from("--no-config")];

        assert_eq!(
            args_of("rumdl-format", 2, &project),
            [
                "fmt",
                "--no-cache",
                "--no-exclude",
                "--extend-disable=MD041,MD051,MD057",
                "--no-config",
                "--stdin-filename",
                "/p/docs/a.md",
                "-"
            ]
        );
        assert_eq!(
            args_of("rumdl", 2, &project),
            [
                "check",
                "--no-cache",
                "--no-exclude",
                "--output-format=json",
                "--extend-disable=MD041,MD051,MD057",
                "--no-config",
                "--stdin",
                "--stdin-filename",
                "/p/docs/a.md"
            ]
        );
    }

    #[test]
    fn rumdl_is_told_a_markdown_doc_by_its_own_path_and_any_other_doc_by_a_virtual_one() {
        let name_for = |doc: &str| {
            cartridge("rumdl-format")
                .args(&Invocation {
                    doc: Path::new(doc),
                    tag: "",
                    index: 3,
                    edition: Edition::E2024,
                    dialect: Dialect::Bash,
                    project: &[],
                    emit: None,
                    externs: &[],
                    fragment: None,
                })
                .get(5)
                .cloned()
        };

        assert_eq!(name_for("/p/docs/a.md"), Some("/p/docs/a.md".into()));
        assert_eq!(
            name_for("/p/src/lib.rs"),
            Some(Path::new("/p/src").join("lib.rs.3.md").into_os_string())
        );
    }

    #[test]
    fn rumdls_cached_results_are_keyed_on_every_config_it_can_be_given() {
        let keyed = cartridge("rumdl").config_files;

        assert!(
            fixed::RUMDL_CONFIG_FILES
                .iter()
                .all(|name| keyed.contains(name)),
            "{keyed:?}"
        );
    }

    #[test]
    fn ruff_formats_stdin_under_the_blocks_virtual_path_without_a_cache() {
        assert_eq!(
            args_of("ruff-format", 2, &[]),
            [
                "format",
                "--no-cache",
                "--stdin-filename",
                &*docs("a.md.2.py"),
                "-"
            ]
        );
    }

    #[test]
    fn black_formats_stdin_quietly_under_the_blocks_virtual_path() {
        assert_eq!(
            args_of("black", 1, &[]),
            ["--quiet", "--stdin-filename", &*docs("a.md.1.py"), "-"]
        );
    }

    #[test]
    fn ruff_checks_stdin_under_the_virtual_path_with_the_relaxations_and_no_fix() {
        let args = args_of("ruff", 2, &[]);

        assert_eq!(
            args,
            [
                "check",
                "--no-cache",
                "--output-format=json",
                "--stdin-filename",
                &*docs("a.md.2.py"),
                "--ignore",
                "F821,F401,F841",
                "-"
            ]
        );
        assert!(!args.iter().any(|arg| arg == "--fix"), "{args:?}");
    }

    #[test]
    fn shellcheck_reads_stdin_in_json1_with_no_rc_and_the_relaxations() {
        let args = args_of("shellcheck", 2, &[]);

        assert_eq!(
            args,
            [
                "-s",
                "bash",
                "--norc",
                "--format=json1",
                "--exclude=SC2154,SC2034,SC1091",
                "-"
            ]
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg == "-x" || arg == "--external-sources"),
            "{args:?}"
        );
    }

    #[test]
    fn each_dialect_becomes_shellchecks_shell_flag() {
        let shell = |dialect| {
            cartridge("shellcheck")
                .args(&Invocation {
                    doc: Path::new("/p/a.md"),
                    tag: "",
                    index: 1,
                    edition: Edition::E2024,
                    dialect,
                    project: &[],
                    emit: None,
                    externs: &[],
                    fragment: None,
                })
                .into_iter()
                .take(2)
                .collect::<Vec<_>>()
        };

        assert_eq!(shell(Dialect::Bash), ["-s", "bash"]);
        assert_eq!(shell(Dialect::Posix), ["-s", "sh"]);
        assert_eq!(shell(Dialect::Mksh), ["-s", "ksh"]);
    }

    #[test]
    fn clippy_gets_the_block_on_stdin_with_its_edition_metadata_path_and_relaxations_last() {
        let lints = ["-W".into(), "clippy::pedantic".into()];
        let invocation = Invocation {
            doc: Path::new("/p/docs/a.md"),
            tag: "",
            index: 1,
            edition: Edition::E2021,
            dialect: Dialect::Bash,
            project: &lints,
            emit: Some(Path::new("/c/emit/1-0/block.rmeta")),
            externs: &[],
            fragment: None,
        };

        let args = cartridge("clippy").args(&invocation);

        assert_eq!(
            args,
            [
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--emit=metadata=/c/emit/1-0/block.rmeta",
                "--error-format=json",
                "-W",
                "clippy::pedantic",
                "-A",
                "unused",
                "-A",
                "dead_code",
                "-A",
                "missing_docs",
                "-A",
                "clippy::missing_docs_in_private_items",
                "-",
            ]
        );
        for refused in ["--extern", "-L", "--cfg"] {
            assert!(!args.iter().any(|arg| arg == refused), "{args:?}");
        }
        let key = cartridge("clippy").args(&Invocation {
            emit: None,
            tag: "",
            ..invocation
        });
        assert!(
            key.contains(&"--emit=metadata".into()),
            "a cache key names no metadata path"
        );
    }

    #[test]
    fn clippy_gets_the_project_builds_externs_after_the_error_format_and_before_the_lints() {
        let lints = ["-W".into(), "clippy::pedantic".into()];
        let externs = [
            "--extern".into(),
            "proj=/t/debug/deps/libproj-1.rmeta".into(),
            "-L".into(),
            "dependency=/t/debug/deps".into(),
        ];

        let args = cartridge("clippy").args(&Invocation {
            doc: Path::new("/p/docs/a.md"),
            index: 1,
            edition: Edition::E2021,
            dialect: Dialect::Bash,
            project: &lints,
            emit: None,
            tag: "",
            externs: &externs,
            fragment: None,
        });

        let at = args
            .iter()
            .position(|arg| arg == "--error-format=json")
            .expect("clippy should get its error format");
        assert_eq!(
            args.get(at + 1..at + 7),
            Some(
                [
                    "--extern",
                    "proj=/t/debug/deps/libproj-1.rmeta",
                    "-L",
                    "dependency=/t/debug/deps",
                    "-W",
                    "clippy::pedantic",
                ]
                .map(OsString::from)
                .as_slice()
            ),
            "{args:?}"
        );
        assert_eq!(args.last(), Some(&OsString::from("-")), "{args:?}");
    }

    /// Writes an executable `#!/bin/sh` stub named `name` in `dir`.
    #[cfg(unix)]
    fn stub(dir: &Path, name: &str) {
        fs::create_dir_all(dir).expect("the stub directory should be creatable");
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").expect("the stub should be writable");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
    }

    /// A [`ToolEnv`] whose `PATH` is `entries` joined.
    fn env_with_path(entries: &[&Path]) -> ToolEnv {
        let path = std::env::join_paths(entries).expect("the fixture PATH should join");
        ToolEnv::from_vars(|name| (name == "PATH").then(|| path.clone()))
    }

    /// A fresh directory holding a `root` project directory and an `outside` one.
    fn dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&root).expect("the root should be creatable");
        fs::create_dir_all(&outside).expect("the outside directory should be creatable");
        (dir, root, outside)
    }

    #[cfg(unix)]
    #[test]
    fn a_root_bin_dir_is_searched_before_path_and_its_match_is_the_checkouts_choice() {
        let (_dir, root, outside) = dirs();
        stub(&root.join("node_modules/.bin"), "prettier");
        stub(&outside, "prettier");
        let env = env_with_path(&[&outside]);

        let found = find_in("prettier", &fixed::NODE_BINS, &env, &root);
        let real = root
            .join("node_modules/.bin/prettier")
            .canonicalize()
            .expect("the stub should resolve");
        assert_eq!(found, Found::InsideRoot(real), "the bin dir comes first");
        assert!(
            matches!(find("prettier", &env, &root), Found::OnPath(_)),
            "without bin dirs only PATH is searched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_root_bin_dir_entry_linked_outside_is_still_the_checkouts_choice() {
        let (_dir, root, outside) = dirs();
        stub(&outside, "eslint");
        let bin = root.join("node_modules/.bin");
        fs::create_dir_all(&bin).expect("the bin dir should be creatable");
        symlink(outside.join("eslint"), bin.join("eslint")).expect("the symlink should be made");

        let found = find_in("eslint", &fixed::NODE_BINS, &env_with_path(&[]), &root);
        assert!(matches!(found, Found::InsideRoot(_)), "{found:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_tool_in_no_bin_dir_falls_back_to_path() {
        let (_dir, root, outside) = dirs();
        stub(&outside, "pylint");

        let found = find_in(
            "pylint",
            &fixed::VENV_BINS,
            &env_with_path(&[&outside]),
            &root,
        );
        assert!(matches!(found, Found::OnPath(_)), "{found:?}");
    }

    #[test]
    fn only_pylint_prettier_and_eslint_search_a_bin_dir_and_run_from_the_root_so_are_never_cached()
    {
        for tool in &CARTRIDGES {
            let (bins, cwd): (&[FixedPath], _) = match tool.name {
                "pylint" => (&fixed::VENV_BINS, Cwd::Root),
                "prettier" | "eslint" => (&fixed::NODE_BINS, Cwd::Root),
                _ => (&[], Cwd::Neutral),
            };
            assert_eq!(tool.bins(), bins, "{}", tool.name);
            assert_eq!(tool.cwd(), cwd, "{}", tool.name);
            assert_eq!(tool.cacheable(), cwd != Cwd::Root, "{}", tool.name);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_entry_is_skipped() {
        let (_dir, root, _outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        let env = ToolEnv::from_vars(|name| (name == "PATH").then(|| OsString::from("bin")));

        assert_eq!(find("shfmt", &env, &root), Found::Missing);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_is_not_executable_is_skipped() {
        let (_dir, root, outside) = dirs();
        fs::write(outside.join("shfmt"), "#!/bin/sh\n").expect("the file should be writable");

        assert_eq!(
            find("shfmt", &env_with_path(&[&outside]), &root),
            Found::Missing
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_outside_whose_real_path_is_in_the_root_is_refused() {
        let (_dir, root, outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        symlink(root.join("bin/shfmt"), outside.join("shfmt"))
            .expect("the fixture symlink should be creatable");

        let found = find("shfmt", &env_with_path(&[&outside]), &root);

        assert!(matches!(found, Found::InsideRoot(_)), "{found:?}");
    }

    #[test]
    fn a_tool_on_no_entry_is_missing() {
        let (_dir, root, outside) = dirs();

        assert_eq!(
            find("shfmt", &env_with_path(&[&outside]), &root),
            Found::Missing
        );
        assert_eq!(
            find("shfmt", &ToolEnv::from_vars(|_| None), &root),
            Found::Missing
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_first_match_on_path_wins() {
        let (_dir, root, outside) = dirs();
        stub(&outside, "shfmt");
        stub(&root.join("bin"), "shfmt");
        let real = outside
            .join("shfmt")
            .canonicalize()
            .expect("the stub should resolve");

        assert_eq!(
            find(
                "shfmt",
                &env_with_path(&[&outside, &root.join("bin")]),
                &root
            ),
            Found::OnPath(real)
        );
        assert!(matches!(
            find(
                "shfmt",
                &env_with_path(&[&root.join("bin"), &outside]),
                &root
            ),
            Found::InsideRoot(_)
        ));
    }

    #[test]
    fn an_empty_entry_and_dot_are_not_searched() {
        let absolute = if cfg!(windows) {
            r"C:\usr\bin"
        } else {
            "/usr/bin"
        };
        let path = std::env::join_paths(["", ".", "", "./bin", absolute, ""])
            .expect("the fixture PATH should join");
        let dirs: Vec<_> = search_dirs(&path).collect();

        assert_eq!(dirs, [Path::new(absolute)]);
    }

    #[cfg(unix)]
    #[test]
    fn a_path_directory_that_is_a_symlink_into_the_root_is_refused() {
        let (_dir, root, outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        symlink(root.join("bin"), outside.join("bin"))
            .expect("the fixture symlink should be creatable");

        let found = find("shfmt", &env_with_path(&[&outside.join("bin")]), &root);

        assert!(matches!(found, Found::InsideRoot(_)), "{found:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_root_reached_through_a_symlink_is_judged_by_its_real_path() {
        let (dir, root, outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        stub(&outside, "ruff");
        let alias = dir.path().join("alias");
        symlink(&root, &alias).expect("the fixture symlink should be creatable");

        let inside = find("shfmt", &env_with_path(&[&root.join("bin")]), &alias);
        let elsewhere = find("ruff", &env_with_path(&[&outside]), &alias);

        assert!(matches!(inside, Found::InsideRoot(_)), "{inside:?}");
        assert!(matches!(elsewhere, Found::OnPath(_)), "{elsewhere:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_sibling_root_sharing_the_name_prefix_is_not_inside() {
        let (dir, root, _outside) = dirs();
        let sibling = dir.path().join("root2");
        stub(&sibling.join("bin"), "shfmt");

        let found = find("shfmt", &env_with_path(&[&sibling.join("bin")]), &root);

        assert!(matches!(found, Found::OnPath(_)), "{found:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_same_inode_alias_of_the_root_is_inside() {
        let (dir, root, _outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        let alias = dir.path().join("alias");
        symlink(&root, &alias).expect("the fixture symlink should be creatable");
        let real = root
            .join("bin/shfmt")
            .canonicalize()
            .expect("the stub should resolve");

        assert!(
            !real.starts_with(&alias),
            "the alias should not be a path prefix"
        );
        assert!(under_same_file(&real, &alias));
    }

    /// A frame row of `check --help`: its tool, its kind, its frame and the language its
    /// marker names, `None` for a row every one of the tool's languages reads.
    struct HelpRow<'a> {
        tool: &'a str,
        kind: &'a str,
        frame: &'a str,
        language: Option<Language>,
        line: &'a str,
    }

    impl HelpRow<'_> {
        /// Whether this row lists `frame` as `tool`'s frame for `kind`.
        fn lists(&self, tool: &ToolCartridge, kind: Kind, frame: &str) -> bool {
            self.tool == tool.name
                && self.kind == kind.name()
                && self.frame == frame
                && self
                    .language
                    .is_none_or(|language| language == tool.language)
        }
    }

    #[test]
    fn check_help_lists_exactly_the_frames_the_tools_read() {
        let rows: Vec<HelpRow<'_>> = crate::cli::help::CHECK_LONG_ABOUT
            .lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                let (tool, kind) = (words.next()?, words.next()?);
                let rest = line.trim_start().strip_prefix(tool)?.trim_start();
                let mut columns = rest
                    .strip_prefix(kind)?
                    .split("  ")
                    .map(str::trim)
                    .filter(|column| !column.is_empty());
                let (frame, marker) = (columns.next()?, columns.next().unwrap_or_default());
                let language = [Language::TypeScript, Language::JavaScript]
                    .into_iter()
                    .find(|language| marker.to_lowercase().contains(language.table()));
                (line.starts_with("  ")
                    && CARTRIDGES.iter().any(|known| known.name == tool)
                    && Kind::from_name(kind).is_some())
                .then_some(HelpRow {
                    tool,
                    kind,
                    frame,
                    language,
                    line,
                })
            })
            .collect();
        let frames: Vec<(&ToolCartridge, Kind, String)> = CARTRIDGES
            .iter()
            .flat_map(|tool| {
                tool.fragments.iter().filter_map(move |(kind, support)| {
                    let frame = support.frame(*kind)?;
                    let shown = format!("{} ... {}", frame.before, frame.after);
                    Some((tool, *kind, shown.trim_end().to_owned()))
                })
            })
            .collect();

        for (tool, kind, shown) in &frames {
            assert!(
                rows.iter().any(|row| row.lists(tool, *kind, shown)),
                "`check --help` should list {} for {}'s {} frame as `{shown}`",
                tool.name,
                tool.language.table(),
                kind.name()
            );
        }
        for row in &rows {
            assert!(
                frames
                    .iter()
                    .any(|(tool, kind, shown)| row.lists(tool, *kind, shown)),
                "`check --help` lists a frame no tool reads: {}",
                row.line
            );
        }
    }

    #[test]
    fn biome_format_reads_each_kind_in_biome_lints_frame_for_it() {
        for language in [Language::TypeScript, Language::JavaScript] {
            let of = |name: &str| {
                CARTRIDGES
                    .iter()
                    .find(|tool| tool.name == name && tool.language == language)
                    .expect("the cartridges should include biome for TypeScript and JavaScript")
            };
            let (format, lint) = (of("biome-format"), of("biome-lint"));
            let kinds: Vec<Kind> = format.fragments.iter().map(|(kind, _)| *kind).collect();

            assert_eq!(
                kinds,
                [
                    Kind::Statements,
                    Kind::Entries,
                    Kind::Children,
                    Kind::Members
                ],
                "{}",
                language.table()
            );
            for kind in kinds {
                let pair = |tool: &ToolCartridge| {
                    Frame::of(tool, Some(kind)).map(|frame| (frame.before, frame.after))
                };
                assert_eq!(
                    pair(format),
                    pair(lint),
                    "{} {}",
                    language.table(),
                    kind.name()
                );
            }
        }
    }

    #[test]
    fn biome_lint_reads_children_only_in_a_jsx_flavor() {
        let lint = CARTRIDGES
            .iter()
            .find(|tool| tool.name == "biome-lint" && tool.language == Language::TypeScript)
            .expect("the cartridges should include biome-lint for TypeScript");

        assert!(lint.supports(Kind::Children, "tsx"));
        assert!(!lint.supports(Kind::Children, "ts"));
        assert!(lint.supports(Kind::Entries, "ts"));
    }

    #[test]
    fn biome_lint_leaves_no_rule_off_for_a_framed_block() {
        let plain = cartridge("biome-lint").args(&Invocation {
            doc: Path::new("/p/docs/a.md"),
            tag: "ts",
            index: 1,
            edition: Edition::E2024,
            dialect: Dialect::Bash,
            project: &[],
            emit: None,
            externs: &[],
            fragment: None,
        });

        for kind in Kind::ALL {
            let framed = cartridge("biome-lint").args(&Invocation {
                doc: Path::new("/p/docs/a.md"),
                tag: "ts",
                index: 1,
                edition: Edition::E2024,
                dialect: Dialect::Bash,
                project: &[],
                emit: None,
                externs: &[],
                fragment: Some(kind),
            });
            assert_eq!(framed, plain, "{}", kind.name());
        }
    }
}
