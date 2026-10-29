//! `supdoc check`. Not built yet.

use super::args::CheckArgs;
use super::{CliError, Context, Outcome};

/// Runs `supdoc check`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(_args: &CheckArgs, _context: &Context) -> Result<Outcome, CliError> {
    Err(CliError::not_built("check"))
}
