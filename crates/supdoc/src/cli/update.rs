//! `supdoc update`. Not built yet.

use super::args::UpdateArgs;
use super::{CliError, Context, Outcome};
use crate::output::Output;

/// Runs `supdoc update`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(
    _args: &UpdateArgs,
    _context: &Context,
    _output: &mut Output,
) -> Result<Outcome, CliError> {
    Err(CliError::not_built("update"))
}

/// The JSON Schema of `supdoc update`'s `--json` output.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn schema() -> Result<schemars::Schema, CliError> {
    Err(CliError::not_built("update"))
}
