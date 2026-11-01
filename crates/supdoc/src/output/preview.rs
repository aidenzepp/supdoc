//! The closing note of a preview: what to run to apply it.

/// The note a write command's preview ends with, naming how to apply what was previewed.
///
/// `command` is the exact shell command that applies it, `--yes` included. When the run was a dry
/// run (`--dry-run` or `SUPDOC_DRY_RUN`, which beats `--yes`), the note also says to drop that
/// first, because adding `--yes` alone would only preview again.
#[must_use]
pub fn preview_note(dry_run: bool, command: &str) -> String {
    if dry_run {
        format!(
            "Nothing written. Drop --dry-run (or unset SUPDOC_DRY_RUN) and add --yes to apply, as \
             in `{command}`."
        )
    } else {
        format!("Nothing written. Run `{command}` to apply.")
    }
}
