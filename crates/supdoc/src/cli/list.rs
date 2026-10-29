//! `supdoc list`. Not built yet.

use super::args::ListArgs;
use super::{CliError, Context, Outcome};
use crate::output::Output;

/// Runs `supdoc list`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(
    _args: &ListArgs,
    _context: &Context,
    _output: &mut Output,
) -> Result<Outcome, CliError> {
    Err(CliError::not_built("list"))
}

/// The JSON Schema of `supdoc list`'s `--json` output.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn schema() -> Result<schemars::Schema, CliError> {
    Err(CliError::not_built("list"))
}
