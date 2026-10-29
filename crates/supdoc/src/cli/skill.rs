//! `supdoc skill install`. Not built yet.

use super::args::SkillInstallArgs;
use super::{CliError, Context, Outcome};

/// Runs `supdoc skill install`.
///
/// # Errors
///
/// Returns `cli/not-built` until the command is built.
pub fn run(_args: &SkillInstallArgs, _context: &Context) -> Result<Outcome, CliError> {
    Err(CliError::not_built("skill install"))
}
