//! `supdoc namespace rename`. Not built yet.

use super::args::RenameArgs;
use super::{CliError, Context, Outcome};
use crate::output::Output;

/// Runs `supdoc namespace rename`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(
    _args: &RenameArgs,
    _context: &Context,
    _output: &mut Output,
) -> Result<Outcome, CliError> {
    Err(CliError::not_built("namespace rename"))
}

/// The JSON Schema of `supdoc namespace rename`'s `--json` output.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn schema() -> Result<schemars::Schema, CliError> {
    Err(CliError::not_built("namespace rename"))
}
