//! `supdoc config show` and `supdoc namespace list`. Not built yet.

use super::args::Inspect;
use super::{CliError, Context, Outcome};

/// Runs `supdoc config show` or `supdoc namespace list`, as `inspect` says.
///
/// # Errors
///
/// Returns `cli/not-built` until the commands are built.
pub fn run(inspect: Inspect, _context: &Context) -> Result<Outcome, CliError> {
    Err(CliError::not_built(match inspect {
        Inspect::ConfigShow => "config show",
        Inspect::NamespaceList => "namespace list",
    }))
}
