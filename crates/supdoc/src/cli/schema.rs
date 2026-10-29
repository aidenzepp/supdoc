//! `supdoc schema`. Not built yet.

use super::args::SchemaArgs;
use super::{CliError, Context, Outcome};

/// Runs `supdoc schema`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(_args: &SchemaArgs, _context: &Context) -> Result<Outcome, CliError> {
    Err(CliError::not_built("schema"))
}
