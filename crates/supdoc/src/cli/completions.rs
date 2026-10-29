//! `supdoc completions`. Not built yet.

use super::args::CompletionsArgs;
use super::{CliError, Context, Outcome};
use crate::output::Output;

/// Runs `supdoc completions`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(
    _args: &CompletionsArgs,
    _context: &Context,
    _output: &mut Output,
) -> Result<Outcome, CliError> {
    Err(CliError::not_built("completions"))
}
