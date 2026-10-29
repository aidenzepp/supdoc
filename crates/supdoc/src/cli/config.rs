//! `supdoc config show` and `supdoc namespace list`. Not built yet.

use super::args::Inspect;
use super::{CliError, Context, Outcome};
use crate::output::Output;

/// Runs `supdoc config show` or `supdoc namespace list`, as `inspect` says.
///
/// # Errors
///
/// Returns `cli/not-built` until the commands are built.
pub fn run(
    inspect: Inspect,
    _context: &Context,
    _output: &mut Output,
) -> Result<Outcome, CliError> {
    Err(CliError::not_built(command(inspect)))
}

/// The JSON Schema of the `--json` output of `supdoc config show` or `supdoc namespace list`, as
/// `inspect` says.
///
/// # Errors
///
/// Returns `cli/not-built` until the commands are built.
pub fn schema(inspect: Inspect) -> Result<schemars::Schema, CliError> {
    Err(CliError::not_built(command(inspect)))
}

/// The command `inspect` names, as a user types it after `supdoc`.
const fn command(inspect: Inspect) -> &'static str {
    match inspect {
        Inspect::ConfigShow => "config show",
        Inspect::NamespaceList => "namespace list",
    }
}
