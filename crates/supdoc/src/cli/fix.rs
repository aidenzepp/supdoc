//! `supdoc fix`. Not built yet.

use super::args::FixArgs;
use super::{CliError, Context, Outcome};

/// Runs `supdoc fix`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(_args: &FixArgs, _context: &Context) -> Result<Outcome, CliError> {
    Err(CliError::not_built("fix"))
}
