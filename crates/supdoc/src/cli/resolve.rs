//! `supdoc resolve`. Not built yet.

use super::args::ResolveArgs;
use super::{CliError, Context, Outcome};
use crate::output::Output;

/// Runs `supdoc resolve`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(
    _args: &ResolveArgs,
    _context: &Context,
    _output: &mut Output,
) -> Result<Outcome, CliError> {
    Err(CliError::not_built("resolve"))
}

/// The JSON Schema of `supdoc resolve`'s `--json` output.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn schema() -> Result<schemars::Schema, CliError> {
    Err(CliError::not_built("resolve"))
}
