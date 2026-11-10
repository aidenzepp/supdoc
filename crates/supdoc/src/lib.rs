//! supdoc tracks links from documentation to code and reports when the linked code changes.
//!
//! The crate root. The library is the `supdoc` binary's implementation, not a stable API: its
//! public surface is [`main`], the binary's one entry point, and [`testing`], the seam the
//! integration tests use to reach codes no binary run can print. Every other module is
//! crate-private.

#![cfg_attr(
    test,
    allow(
        clippy::missing_errors_doc,
        clippy::missing_panics_doc,
        reason = "tests report failures rather than document callable contracts"
    )
)]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "tests build fixture and hostile paths with raw joins on purpose"
    )
)]

/// The process exit status.
pub(crate) mod exit;

/// The shared "did you mean" rule: one cutoff, one distance metric, a length prefilter.
pub(crate) mod suggest;

/// The shell commands a help names to fix a path, for the host's shell.
pub(crate) mod remedies;

/// Absolute and project-relative paths, normalized lexically.
pub(crate) mod path;

/// Links from documentation to code: keys, references and destinations.
pub(crate) mod link;

/// Namespace names that prefix link destinations.
pub(crate) mod namespace;

/// Content hashes of linked code.
pub(crate) mod hash;

/// Findings: severity, location, snippet and suggested fix.
pub(crate) mod finding;

/// The state of a link compared with the lockfile.
pub(crate) mod state;

/// Loading `supdoc.config.toml` and its `extends` chain.
pub(crate) mod config;

/// The `[limits]` ceilings: units, precedence and where each value came from.
pub(crate) mod limits;

/// The `supdoc.lock` lockfile: its in-memory model and its file format.
pub(crate) mod lock {
    /// The in-memory lockfile.
    pub(crate) mod model;

    /// Parsing the lockfile text.
    pub(crate) mod parse;

    /// Rendering the lockfile text.
    pub(crate) mod render;

    /// Reading and atomically writing the lockfile.
    pub(crate) mod io;

    /// Lockfile errors.
    pub(crate) mod error;
}

/// Reading source files: parsing, symbols, hashing and link extraction.
pub(crate) mod read {
    /// The `Cartridge` trait that plugs a language into supdoc.
    pub(crate) mod cartridge;

    /// A source file read into memory.
    pub(crate) mod source;

    /// Cartridges driven by tree-sitter query files.
    pub(crate) mod query;

    /// Symbol tables built from `tags.scm` captures, and symbol resolution.
    pub(crate) mod symbols;

    /// Hashing a symbol's bytes or a whole file.
    pub(crate) mod hash;

    /// The built-in language registry.
    pub(crate) mod registry;

    /// Reading and resolution errors.
    pub(crate) mod error;

    /// Finding links in a file's Markdown regions.
    pub(crate) mod links;
}

/// Built-in language cartridges.
pub(crate) mod cartridges {
    /// Rust, the reference cartridge.
    pub(crate) mod rust;

    /// TypeScript.
    pub(crate) mod ts;

    /// TSX.
    pub(crate) mod tsx;

    /// JavaScript.
    pub(crate) mod js;

    /// Python.
    pub(crate) mod python;

    /// Go.
    pub(crate) mod go;

    /// Bash.
    pub(crate) mod bash;

    /// The key-path symbol model the JSON, YAML and TOML cartridges share.
    pub(crate) mod data;

    /// TOML, a link target only.
    pub(crate) mod toml;

    /// JSON, a link target only.
    pub(crate) mod json;

    /// YAML, a link target only.
    pub(crate) mod yaml;

    /// Markdown.
    pub(crate) mod markdown {
        /// The Markdown cartridge.
        pub(crate) mod cartridge;

        /// Headings as GitHub anchors and dotted symbol paths.
        pub(crate) mod symbols;

        /// The Markdown regions where links are read.
        pub(crate) mod regions;
    }
}

/// Scanning a project for links and the state of their targets.
pub(crate) mod scan {
    /// The scan result.
    pub(crate) mod model;

    /// Walking the project tree for source files.
    pub(crate) mod walk;

    /// The ignore engine behind the walk.
    mod ignore;

    /// Finding a directory's git exclude file and a worktree's common git directory.
    pub(crate) mod git;

    /// Judging explicitly named paths.
    mod explicit;

    /// Assembling a scan from sources, links and targets.
    pub(crate) mod assemble;

    /// Grouping links by target and resolving each target once.
    pub(crate) mod targets;
}

/// Comparing a scan with the lockfile and planning changes.
pub(crate) mod check {
    /// The check report.
    pub(crate) mod model;

    /// Reconciling scanned links with lockfile entries.
    pub(crate) mod reconcile;

    /// Detecting links whose target moved.
    pub(crate) mod moves;

    /// Check outcomes and update plans.
    pub(crate) mod plan;

    /// Planning and applying fixes to documentation and the lockfile.
    pub(crate) mod fix;
}

/// Checking the code blocks in documentation: finding each block, reading its info string and
/// judging it. Kept apart from linking: nothing here reads links or targets, and the lockfile
/// only through its `[code-blocks]` accessors.
pub(crate) mod code_blocks {
    /// A block's info string: its language and attributes.
    pub(crate) mod info;

    /// The fragment kinds a block declares, and how a tool supports one.
    pub(crate) mod fragment;

    /// A code block, and finding the blocks in a file by its language.
    pub(crate) mod block;

    /// Fenced code blocks in Markdown files.
    mod markdown;

    /// Code blocks in Rust doc comments.
    mod doc_comments;

    /// Reading an excerpt's source.
    mod source;

    /// Comparing an excerpt with its source.
    mod excerpt;

    /// Tracking `uses(...)` targets against the lockfile.
    pub(crate) mod uses;

    /// Block states and their counts.
    mod states;

    /// The tool cartridges, the environment tools get, and the binary lookup on `PATH`.
    pub(crate) mod tools;

    /// The hardened spawn of a tool on one block.
    mod runner;

    /// The format step.
    mod format;

    /// Custom `{ command = [...] }` formatters, run in a trusted checkout.
    mod custom;

    /// Rust's format and lint steps.
    mod rust;

    /// Shell's format and lint steps.
    mod shell;

    /// Python's format and lint steps.
    mod python;

    /// pylint's lint step, a project-tier Python linter.
    mod pylint;

    /// prettier's format step, a project-tier TypeScript and JavaScript formatter.
    mod prettier;

    /// eslint's lint step, a project-tier TypeScript and JavaScript linter.
    mod eslint;

    /// TypeScript, JavaScript, JSON and CSS's format and lint steps, run by biome.
    mod biome;
    /// The Makefile's lint step, run by checkmake.
    mod checkmake;
    /// Dockerfile's format step, run by dockerfmt.
    mod dockerfmt;
    /// Dockerfile's lint step, run by hadolint.
    mod hadolint;
    /// The Makefile's format step, run by mbake.
    mod mbake;
    /// Markdown's format and lint steps, run by rumdl.
    mod rumdl;
    /// SQL's format and lint steps, run by sqruff.
    mod sqruff;
    /// HTML's format step, run by superhtml.
    mod superhtml;
    /// TOML's format and lint steps, run by tombi.
    mod tombi;
    /// YAML's format step, run by yamlfmt.
    mod yaml;

    /// Go's format and lint steps, run by gofmt and go vet.
    mod go;

    /// Reading the Rust toolchain a project pins.
    mod toolchain;

    /// Building the project crate clippy links a trusted block against.
    mod project;

    /// The result cache.
    mod cache;

    /// The lint step.
    mod lint;

    /// The per-block pipeline `check` runs.
    pub(crate) mod evaluate;
}

/// Rendering reports as text, JSON and JSONL.
pub(crate) mod output;

/// The command-line interface: arguments and commands.
pub(crate) mod cli;

/// The trust store: the checkouts the operator trusts to run code they control.
pub(crate) mod trust;

/// The process boundary: reading the environment and turning a run into an exit code.
mod process;

pub mod testing;

/// Runs the `supdoc` command with the process's arguments and environment.
///
/// The `supdoc` binary is this one call.
#[must_use]
pub fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(process::exit_code())
}
