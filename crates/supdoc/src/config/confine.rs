//! Where a path really lies once symlinks are followed, so supdoc can stay inside the project root.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// How many dangling symlinks one lookup follows before it gives up, as the OS limits loops.
const MAX_DANGLING_HOPS: u8 = 40;

/// The real location of `path`: every symlink along it followed, with any missing tail appended.
///
/// A dangling symlink is followed to where it points, so a link to a missing file outside the
/// root still lands outside. `None` when that can't be worked out (a symlink loop, or a `..`
/// below a missing name); callers treat that as outside.
///
/// Effect: reads file metadata and symlinks along `path`.
pub(crate) fn real_location(path: &Path) -> Option<PathBuf> {
    real_location_within(path, MAX_DANGLING_HOPS)
}

/// [`real_location`], following at most `hops` more dangling symlinks.
fn real_location_within(path: &Path, hops: u8) -> Option<PathBuf> {
    if let Ok(real) = fs::canonicalize(path) {
        return Some(real);
    }
    // Climb to the deepest entry that exists, remembering the missing names below it.
    let mut existing = path;
    let mut missing = Vec::new();
    while fs::symlink_metadata(existing).is_err() {
        missing.push(normal_name(existing)?);
        existing = existing.parent()?;
    }
    let real_existing = if let Ok(real) = fs::canonicalize(existing) {
        real
    } else {
        // The entry exists but can't be resolved: a dangling symlink. Follow it by hand.
        let next = hops.checked_sub(1)?;
        let pointee = fs::read_link(existing).ok()?;
        let parent = fs::canonicalize(existing.parent()?).ok()?;
        real_location_within(&parent.join(pointee), next)?
    };
    Some(
        missing
            .iter()
            .rev()
            .fold(real_existing, |real, name| real.join(name)),
    )
}

/// The last component of `path` when it is an ordinary name, not `..`, `.` or a root.
fn normal_name(path: &Path) -> Option<&std::ffi::OsStr> {
    match path.components().next_back()? {
        Component::Normal(name) => Some(name),
        Component::Prefix(_) | Component::RootDir | Component::CurDir | Component::ParentDir => {
            None
        }
    }
}
