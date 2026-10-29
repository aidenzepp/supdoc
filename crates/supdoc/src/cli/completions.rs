//! `supdoc completions`. Not built yet.

use super::args::CompletionsArgs;
use super::{CliError, Context, Outcome};

/// Runs `supdoc completions`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(_args: &CompletionsArgs, _context: &Context) -> Result<Outcome, CliError> {
    Err(CliError::not_built("completions"))
}
