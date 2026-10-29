//! supdoc tracks links from documentation to code and reports when the linked code changes.
//!
//! Every module is declared here up front, so later work adds files without editing this one.

#![cfg_attr(
    test,
    allow(
        clippy::missing_errors_doc,
        clippy::missing_panics_doc,
        reason = "tests report failures rather than document callable contracts"
    )
)]

/// Absolute and project-relative paths, normalized lexically.
pub mod path;

/// Links from documentation to code: keys, references and destinations.
pub mod link;

/// Namespace names that prefix link destinations.
pub mod namespace;

/// Content hashes of linked code.
pub mod hash;

/// Findings: severity, location, snippet and suggested fix.
pub mod finding;

/// The state of a link compared with the lockfile.
pub mod state;

/// Loading `supdoc.config.toml` and its `extends` chain.
pub mod config;

/// The `supdoc.lock` lockfile: its in-memory model and its file format.
pub mod lock {
    /// The in-memory lockfile.
    pub mod model;

    /// Parsing the lockfile text.
    pub mod parse;

    /// Rendering the lockfile text.
    pub mod render;

    /// Reading and atomically writing the lockfile.
    pub mod io;

    /// Lockfile errors.
    pub mod error;
}

/// Reading source files: parsing, symbols, hashing and link extraction.
pub mod read {
    /// The `Cartridge` trait that plugs a language into supdoc.
    pub mod cartridge;

    /// A source file read into memory.
    pub mod source;

    /// Cartridges driven by tree-sitter query files.
    pub mod query;

    /// Symbol tables built from `tags.scm` captures, and symbol resolution.
    pub mod symbols;

    /// Hashing a symbol's bytes or a whole file.
    pub mod hash;

    /// The built-in language registry.
    pub mod registry;

    /// Reading and resolution errors.
    pub mod error;

    /// Finding links in a file's Markdown regions.
    pub mod links;
}

/// Built-in language cartridges.
pub mod cartridges {
    /// Rust, the reference cartridge.
    pub mod rust;

    /// TypeScript.
    pub mod ts;

    /// TSX.
    pub mod tsx;

    /// JavaScript.
    pub mod js;

    /// Python.
    pub mod python;

    /// Go.
    pub mod go;

    /// Bash.
    pub mod bash;

    /// Markdown.
    pub mod markdown {
        /// The Markdown cartridge.
        pub mod cartridge;

        /// Headings as GitHub anchors and dotted symbol paths.
        pub mod symbols;

        /// The Markdown regions where links are read.
        pub mod regions;
    }
}

/// Scanning a project for links and the state of their targets.
pub mod scan {
    /// The scan result.
    pub mod model;

    /// Walking the project tree for source files.
    pub mod walk;

    /// Assembling a scan from sources, links and targets.
    pub mod assemble;

    /// Grouping links by target and resolving each target once.
    pub mod targets;
}

/// Comparing a scan with the lockfile and planning changes.
pub mod check {
    /// The check report.
    pub mod model;

    /// Reconciling scanned links with lockfile entries.
    pub mod reconcile;

    /// Detecting links whose target moved.
    pub mod moves;

    /// Check outcomes and update plans.
    pub mod plan;

    /// Planning and applying fixes to documentation and the lockfile.
    pub mod fix;
}

/// Rendering reports as text, JSON and JSONL.
pub mod output;

/// The command-line interface: arguments and commands.
pub mod cli;
