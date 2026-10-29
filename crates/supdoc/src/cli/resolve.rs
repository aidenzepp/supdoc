//! `supdoc resolve`. Not built yet.

use super::args::ResolveArgs;
use super::{CliError, Context, Outcome};

/// Runs `supdoc resolve`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(_args: &ResolveArgs, _context: &Context) -> Result<Outcome, CliError> {
    Err(CliError::not_built("resolve"))
}
