//! Shell's format and lint steps.

use std::ffi::OsString;
use std::path::Path;

/// `shfmt`'s arguments for the `index`th block (1-based) of `doc`, the doc's absolute path:
/// `--filename <doc>/<index>.sh`, so config discovery starts at the doc while the block itself
/// arrives on stdin.
#[must_use]
pub(crate) fn format_args(doc: &Path, index: usize) -> Vec<OsString> {
    vec![
        OsString::from("--filename"),
        doc.join(format!("{index}.sh")).into_os_string(),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::format_args;

    #[test]
    fn the_filename_names_the_block_by_its_index_in_the_doc() {
        assert_eq!(
            format_args(Path::new("/p/docs/a.md"), 3),
            ["--filename", "/p/docs/a.md/3.sh"]
        );
    }
}
