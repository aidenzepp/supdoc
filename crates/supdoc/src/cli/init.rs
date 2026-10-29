//! `supdoc init`. Not built yet.

use super::args::InitArgs;
use super::{CliError, Context, Outcome};

/// Runs `supdoc init`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(_args: &InitArgs, _context: &Context) -> Result<Outcome, CliError> {
    Err(CliError::not_built("init"))
}
