//! `supdoc check`. Not built yet.

use super::args::CheckArgs;
use super::{CliError, Context, Outcome};
use crate::output::Output;

/// Runs `supdoc check`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(
    _args: &CheckArgs,
    _context: &Context,
    _output: &mut Output,
) -> Result<Outcome, CliError> {
    Err(CliError::not_built("check"))
}
