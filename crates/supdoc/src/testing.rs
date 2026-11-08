//! The test seam: builds the messages for codes no `supdoc` run can print reliably.
//!
//! Each of these codes needs a condition the binary can't be made to meet on demand: a tree-sitter
//! failure, a built-in query that doesn't compile, or a file edited between planning and writing.
//! The integration tests check their code and help through these functions instead. The seam
//! returns plain strings, so no internal type leaves the crate.
//!
//! This is not a stable API: it changes whenever the tests need it to.

use std::path::Path;

use crate::check::fix::{apply_fixes, plan_fixes};
use crate::config::Config;
use crate::path::{AbsolutePath, Component};
use crate::read::cartridge::Languages;
use crate::read::error::{CartridgeLoadError, ReadError, ReadRole};
use crate::scan::assemble;
use crate::scan::model::Scope;
use crate::scan::walk::GlobalExcludes;

/// A message's code and the help it prints, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The code, such as `read-internal`.
    pub code: String,
    /// The help line.
    pub help: Option<String>,
}

/// The `clap` definition of the `supdoc` command line.
#[must_use]
pub fn command() -> clap::Command {
    <crate::cli::args::Cli as clap::CommandFactory>::command()
}

/// The exit status of a usage error.
#[must_use]
pub const fn usage_exit_code() -> u8 {
    crate::exit::ExitStatus::Usage.code()
}

/// The exit status of a supdoc bug.
#[must_use]
pub const fn bug_exit_code() -> u8 {
    crate::exit::ExitStatus::Bug.code()
}

/// `read-internal`: tree-sitter returned no tree for a scanned source.
#[must_use]
pub fn read_internal() -> Option<Message> {
    let path = AbsolutePath::name_root().beneath(&Component::new("lib.rs")?);
    let error = ReadError::Internal {
        path,
        reason: "tree-sitter returned no tree",
    };
    Some(Message {
        code: error.code().to_owned(),
        help: Some(error.help(ReadRole::ScannedSource, "file")),
    })
}

/// `read-cartridge-load`: a built-in query or grammar doesn't load.
#[must_use]
pub fn read_cartridge_load() -> Message {
    Message {
        code: CartridgeLoadError::code().to_owned(),
        help: Some(CartridgeLoadError::help()),
    }
}

/// `cli-rename-file-changed`: a doc changed between planning and applying a namespace rename.
#[must_use]
pub fn rename_file_changed() -> Message {
    let error = crate::cli::rename::file_changed(
        "docs/guide.md",
        "supdoc namespace rename core base --yes",
    );
    Message {
        code: error.code().to_owned(),
        help: Some(error.help().to_owned()),
    }
}

/// `fix-file-changed`: scans and plans fixes for the project at `root`, runs `edit`, then applies
/// the plan and returns the first skipped file's finding.
///
/// # Errors
///
/// Returns why the project couldn't be loaded or scanned, or that no file was skipped.
pub fn fix_file_changed(root: &Path, edit: impl FnOnce()) -> Result<Message, String> {
    let root = AbsolutePath::new(root).ok_or("the project root should be absolute")?;
    let config = Config::load(&root).map_err(|error| error.to_string())?;
    let languages = Languages::builtin().map_err(ToString::to_string)?;
    let scan = assemble::run(&config, languages, &Scope::All, &GlobalExcludes::none())
        .map_err(|error| error.to_string())?;
    let plan = plan_fixes(&scan);
    edit();
    let result = apply_fixes(&plan, config.boundary(), "supdoc fix --yes");
    let skipped = result
        .skipped
        .first()
        .ok_or("a doc edited after the scan should be skipped")?;
    Ok(Message {
        code: skipped.finding.code.to_owned(),
        help: skipped.finding.help.clone(),
    })
}
