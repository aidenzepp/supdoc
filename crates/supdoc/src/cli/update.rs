//! `supdoc update`. Not built yet.

use super::args::UpdateArgs;
use super::{CliError, Context, Outcome};

/// Runs `supdoc update`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(_args: &UpdateArgs, _context: &Context) -> Result<Outcome, CliError> {
    Err(CliError::not_built("update"))
}
