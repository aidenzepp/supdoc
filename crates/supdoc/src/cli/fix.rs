//! `supdoc fix`. Not built yet.

use super::args::FixArgs;
use super::{CliError, Context, Outcome};
use crate::output::Output;

/// Runs `supdoc fix`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(_args: &FixArgs, _context: &Context, _output: &mut Output) -> Result<Outcome, CliError> {
    Err(CliError::not_built("fix"))
}

/// The JSON Schema of `supdoc fix`'s `--json` output.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn schema() -> Result<schemars::Schema, CliError> {
    Err(CliError::not_built("fix"))
}
