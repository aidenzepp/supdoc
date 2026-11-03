//! Rust's format and lint steps.

use std::ffi::OsString;
use std::path::Path;

/// `rustfmt`'s arguments for the `index`th block of `doc`: none yet, so Rust blocks stay
/// pending until the Rust unit builds the step.
#[must_use]
pub(crate) const fn format_args(_doc: &Path, _index: usize) -> Option<Vec<OsString>> {
    None
}
