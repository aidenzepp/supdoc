//! `supdoc list`. Not built yet.

use super::args::ListArgs;
use super::{CliError, Context, Outcome};

/// Runs `supdoc list`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(_args: &ListArgs, _context: &Context) -> Result<Outcome, CliError> {
    Err(CliError::not_built("list"))
}
