//! The shell commands a help names to fix a file or directory, in the host's own shell.
//!
//! A help that says `chmod u+r file` is no use on Windows, where the shell is PowerShell. Each
//! remedy here takes a `windows` flag and returns the Unix command the helps always printed or its
//! Windows counterpart. Production code passes [`WINDOWS`], the one place the host is chosen;
//! tests pass both values.

/// Whether this build targets Windows: the one place production chooses the remedy's platform.
pub(crate) const WINDOWS: bool = cfg!(windows);

/// What a remedy gives the current user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// Read a file.
    Read,
    /// Read and search (list) a directory.
    ReadSearch,
    /// Write to a file or directory.
    Write,
}

/// `text` as one shell word: unchanged when it holds only characters the shell never treats
/// specially, otherwise in single quotes, so a help can name a path with spaces and still run.
///
/// A quote inside the text is escaped as the shell expects: `'\''` for a Unix shell and `''` for
/// PowerShell.
pub(crate) fn quote(text: &str, windows: bool) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:,@%".contains(c));
    if plain {
        text.to_owned()
    } else if windows {
        format!("'{}'", text.replace('\'', "''"))
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// The command that gives the current user `access` to `target` (already quoted): `chmod` on Unix,
/// `icacls` on Windows.
pub(crate) fn grant_on(access: Access, target: &str, windows: bool) -> String {
    if windows {
        let rights = match access {
            Access::Read => "R",
            Access::ReadSearch => "RX",
            Access::Write => "W",
        };
        format!("icacls {target} /grant \"${{env:USERNAME}}:({rights})\"")
    } else {
        format!("{} {target}", grant(access, false))
    }
}

/// The command that gives the current user `access`, for a help that does not name the target:
/// `chmod u+r` or `icacls`.
pub(crate) const fn grant(access: Access, windows: bool) -> &'static str {
    if windows {
        "icacls"
    } else {
        match access {
            Access::Read => "chmod u+r",
            Access::ReadSearch => "chmod u+rx",
            Access::Write => "chmod u+w",
        }
    }
}

/// The command that shows what `target` (already quoted) is and who may use it: `ls -l` on Unix,
/// `Get-Item` on Windows.
pub(crate) fn inspect_on(target: &str, windows: bool) -> String {
    format!("{} {target}", inspect(windows))
}

/// The command that shows what a path is, with no target named: `ls -l` or `Get-Item`.
pub(crate) const fn inspect(windows: bool) -> &'static str {
    if windows { "Get-Item" } else { "ls -l" }
}

/// The command that shows a directory itself, not its contents, for `directory` (already quoted):
/// `ls -ld` on Unix, `Get-Item` on Windows.
pub(crate) fn inspect_directory(directory: &str, windows: bool) -> String {
    if windows {
        format!("Get-Item {directory}")
    } else {
        format!("ls -ld {directory}")
    }
}

/// The command that creates `directory` (already quoted) and any missing parents: `mkdir -p` on
/// Unix, `New-Item -ItemType Directory -Force` on Windows.
pub(crate) fn make_directory(directory: &str, windows: bool) -> String {
    if windows {
        format!("New-Item -ItemType Directory -Force {directory}")
    } else {
        format!("mkdir -p {directory}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_on_renders_chmod_for_unix_and_icacls_for_windows() {
        let cases = [
            (
                Access::Read,
                "chmod u+r f",
                "icacls f /grant \"${env:USERNAME}:(R)\"",
            ),
            (
                Access::ReadSearch,
                "chmod u+rx f",
                "icacls f /grant \"${env:USERNAME}:(RX)\"",
            ),
            (
                Access::Write,
                "chmod u+w f",
                "icacls f /grant \"${env:USERNAME}:(W)\"",
            ),
        ];
        for (access, unix, windows) in cases {
            assert_eq!(grant_on(access, "f", false), unix, "{access:?} on Unix");
            assert_eq!(
                grant_on(access, "f", true),
                windows,
                "{access:?} on Windows"
            );
        }
    }

    #[test]
    fn untargeted_grant_renders_chmod_for_unix_and_icacls_for_windows() {
        assert_eq!(grant(Access::Read, false), "chmod u+r");
        assert_eq!(grant(Access::ReadSearch, false), "chmod u+rx");
        assert_eq!(grant(Access::Write, false), "chmod u+w");
        for access in [Access::Read, Access::ReadSearch, Access::Write] {
            assert_eq!(grant(access, true), "icacls", "{access:?} on Windows");
        }
    }

    #[test]
    fn inspect_renders_ls_for_unix_and_get_item_for_windows() {
        assert_eq!(inspect(false), "ls -l");
        assert_eq!(inspect(true), "Get-Item");
        assert_eq!(inspect_on("f", false), "ls -l f");
        assert_eq!(inspect_on("f", true), "Get-Item f");
        assert_eq!(inspect_directory("d", false), "ls -ld d");
        assert_eq!(inspect_directory("d", true), "Get-Item d");
    }

    #[test]
    fn make_directory_renders_mkdir_for_unix_and_new_item_for_windows() {
        assert_eq!(make_directory("d", false), "mkdir -p d");
        assert_eq!(
            make_directory("d", true),
            "New-Item -ItemType Directory -Force d"
        );
    }

    #[test]
    fn quote_escapes_an_inner_quote_as_the_shell_does() {
        assert_eq!(quote("a/b.md", false), "a/b.md");
        assert_eq!(quote("a/b.md", true), "a/b.md");
        assert_eq!(quote("my repo", true), "'my repo'");
        assert_eq!(quote("it's", false), "'it'\\''s'");
        assert_eq!(quote("it's", true), "'it''s'");
        assert_eq!(quote(r"C:\repo\a.rs", true), r"'C:\repo\a.rs'");
    }

    #[test]
    fn the_host_flag_matches_the_build_target() {
        assert_eq!(WINDOWS, cfg!(windows));
    }
}
