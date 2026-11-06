//! The release asset model behind `supdoc upgrade` for a downloaded binary.
//!
//! Pure functions only: no network, no detection, no files written. They name the platform the
//! running binary was built for, the release URLs for a version, parse `SHA256SUMS`, and take the
//! one `supdoc` entry out of a release archive held in memory. The asset contract is
//! `supdoc-X.Y.Z-<triple>.tar.gz` holding `supdoc-X.Y.Z-<triple>/supdoc` (`supdoc.exe` on
//! Windows), listed in one `SHA256SUMS` in `sha256sum` format.

use std::collections::HashSet;
use std::fmt;
use std::io::Read as _;
use std::path::Component;
use std::str::FromStr;

use flate2::read::GzDecoder;
use tar::{Archive, EntryType};

use super::ReleaseVersion;

/// The release page every asset URL lives under.
const RELEASES: &str = "https://github.com/aidenzepp/supdoc/releases";

/// The checksum asset's name.
const SUMS_NAME: &str = "SHA256SUMS";

/// The file name every archive starts with.
const ARCHIVE_PREFIX: &str = "supdoc-";

/// The file name every archive ends with.
const ARCHIVE_SUFFIX: &str = ".tar.gz";

/// The largest `supdoc` binary taken out of an archive, in bytes.
pub(crate) const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;

/// How much the other archive entries (licence, readme) may add to the decompressed stream.
const MAX_EXTRA_BYTES: u64 = 1024 * 1024;

/// A CPU architecture supdoc releases for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arch {
    /// 64-bit x86.
    X86_64,
    /// 64-bit ARM.
    Aarch64,
}

/// An operating system supdoc releases for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Os {
    /// Linux.
    Linux,
    /// macOS.
    Macos,
    /// Windows.
    Windows,
}

/// The `target_env` of a build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Env {
    /// glibc.
    Gnu,
    /// musl.
    Musl,
    /// The MSVC toolchain.
    Msvc,
    /// Any other value, including the empty one macOS reports.
    Unspecified,
}

/// One of the eight platforms a release carries an archive for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Triple {
    /// `x86_64-unknown-linux-gnu`.
    X86_64LinuxGnu,
    /// `aarch64-unknown-linux-gnu`.
    Aarch64LinuxGnu,
    /// `x86_64-unknown-linux-musl`.
    X86_64LinuxMusl,
    /// `aarch64-unknown-linux-musl`.
    Aarch64LinuxMusl,
    /// `x86_64-apple-darwin`.
    X86_64Apple,
    /// `aarch64-apple-darwin`.
    Aarch64Apple,
    /// `x86_64-pc-windows-msvc`.
    X86_64Windows,
    /// `aarch64-pc-windows-msvc`.
    Aarch64Windows,
}

impl Triple {
    /// Every platform, in the order releases list them.
    pub(crate) const ALL: [Self; 8] = [
        Self::X86_64LinuxGnu,
        Self::Aarch64LinuxGnu,
        Self::X86_64LinuxMusl,
        Self::Aarch64LinuxMusl,
        Self::X86_64Apple,
        Self::Aarch64Apple,
        Self::X86_64Windows,
        Self::Aarch64Windows,
    ];

    /// The target triple text used in asset names.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::X86_64LinuxGnu => "x86_64-unknown-linux-gnu",
            Self::Aarch64LinuxGnu => "aarch64-unknown-linux-gnu",
            Self::X86_64LinuxMusl => "x86_64-unknown-linux-musl",
            Self::Aarch64LinuxMusl => "aarch64-unknown-linux-musl",
            Self::X86_64Apple => "x86_64-apple-darwin",
            Self::Aarch64Apple => "aarch64-apple-darwin",
            Self::X86_64Windows => "x86_64-pc-windows-msvc",
            Self::Aarch64Windows => "aarch64-pc-windows-msvc",
        }
    }

    /// The name of the binary inside the archive.
    const fn binary_name(self) -> &'static str {
        match self {
            Self::X86_64Windows | Self::Aarch64Windows => "supdoc.exe",
            _ => "supdoc",
        }
    }
}

impl fmt::Display for Triple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The release platform for a build with these properties, or `None` outside the eight.
pub(crate) const fn triple_for(arch: Arch, os: Os, env: Env) -> Option<Triple> {
    match (arch, os, env) {
        (Arch::X86_64, Os::Linux, Env::Gnu) => Some(Triple::X86_64LinuxGnu),
        (Arch::Aarch64, Os::Linux, Env::Gnu) => Some(Triple::Aarch64LinuxGnu),
        (Arch::X86_64, Os::Linux, Env::Musl) => Some(Triple::X86_64LinuxMusl),
        (Arch::Aarch64, Os::Linux, Env::Musl) => Some(Triple::Aarch64LinuxMusl),
        (Arch::X86_64, Os::Macos, Env::Unspecified) => Some(Triple::X86_64Apple),
        (Arch::Aarch64, Os::Macos, Env::Unspecified) => Some(Triple::Aarch64Apple),
        (Arch::X86_64, Os::Windows, Env::Msvc) => Some(Triple::X86_64Windows),
        (Arch::Aarch64, Os::Windows, Env::Msvc) => Some(Triple::Aarch64Windows),
        _ => None,
    }
}

/// The release platform this binary was built for, or `None` outside the eight.
///
/// It comes from `cfg!`, so a musl build stays musl and a glibc build stays glibc.
pub(crate) const fn running_triple() -> Option<Triple> {
    let arch = if cfg!(target_arch = "x86_64") {
        Arch::X86_64
    } else if cfg!(target_arch = "aarch64") {
        Arch::Aarch64
    } else {
        return None;
    };
    let os = if cfg!(target_os = "linux") {
        Os::Linux
    } else if cfg!(target_os = "macos") {
        Os::Macos
    } else if cfg!(target_os = "windows") {
        Os::Windows
    } else {
        return None;
    };
    let env = if cfg!(target_env = "gnu") {
        Env::Gnu
    } else if cfg!(target_env = "musl") {
        Env::Musl
    } else if cfg!(target_env = "msvc") {
        Env::Msvc
    } else {
        Env::Unspecified
    };
    triple_for(arch, os, env)
}

/// The archive's file name for a version and platform.
pub(crate) fn asset_name(version: &ReleaseVersion, triple: Triple) -> String {
    format!("{ARCHIVE_PREFIX}{version}-{triple}{ARCHIVE_SUFFIX}")
}

/// The URL of the archive for a version and platform.
pub(crate) fn asset_url(version: &ReleaseVersion, triple: Triple) -> String {
    format!(
        "{RELEASES}/download/v{version}/{}",
        asset_name(version, triple)
    )
}

/// The URL of `SHA256SUMS` for a version, or for the latest release when `version` is `None`.
pub(crate) fn sums_url(version: Option<&ReleaseVersion>) -> String {
    match version {
        Some(version) => format!("{RELEASES}/download/v{version}/{SUMS_NAME}"),
        None => format!("{RELEASES}/latest/download/{SUMS_NAME}"),
    }
}

/// One line of `SHA256SUMS`: an archive and the digest it must have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SumEntry {
    /// The 64 lowercase hex digits of the archive's SHA-256.
    pub(crate) digest: String,
    /// The archive's file name.
    pub(crate) name: String,
    /// The version the file name carries.
    pub(crate) version: ReleaseVersion,
    /// The platform the file name carries.
    pub(crate) triple: Triple,
}

/// A parsed `SHA256SUMS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sums {
    /// The lines in file order; never empty.
    entries: Vec<SumEntry>,
}

/// Why `SHA256SUMS` was refused. The line number is 1-based.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SumsError {
    /// The file has no lines.
    #[error("SHA256SUMS is empty: it must list one archive per line")]
    Empty,
    /// A line is not `<64 lowercase hex>  <name>`.
    #[error(
        "SHA256SUMS line {line} is not `<64 lowercase hex digits>  <file name>` with exactly two \
         spaces"
    )]
    Malformed {
        /// The offending line.
        line: usize,
    },
    /// A file name is not `supdoc-X.Y.Z-<triple>.tar.gz` for one of the eight platforms.
    #[error(
        "SHA256SUMS line {line} names `{name}`, which is not a supdoc-X.Y.Z-<triple>.tar.gz archive"
    )]
    UnknownName {
        /// The offending line.
        line: usize,
        /// The file name on that line.
        name: String,
    },
    /// A file name appears on more than one line.
    #[error("SHA256SUMS lists `{name}` more than once (line {line})")]
    Duplicate {
        /// The second line naming the file.
        line: usize,
        /// The repeated file name.
        name: String,
    },
}

impl Sums {
    /// Parses strict `sha256sum` output: LF-terminated lines of 64 lowercase hex digits, two
    /// spaces and an archive name, every name distinct.
    ///
    /// # Errors
    ///
    /// Returns the first line that breaks the format, names a file that is not a release
    /// archive, or repeats a name; an empty file is refused too.
    pub(crate) fn parse(text: &str) -> Result<Self, SumsError> {
        let body = text.strip_suffix('\n').unwrap_or(text);
        if body.is_empty() {
            return Err(SumsError::Empty);
        }
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        for (index, row) in body.split('\n').enumerate() {
            let line = index + 1;
            let (digest, name) = row
                .split_once("  ")
                .filter(|(digest, name)| is_digest(digest) && is_plain_name(name))
                .ok_or(SumsError::Malformed { line })?;
            let (version, triple) =
                parse_asset_name(name).ok_or_else(|| SumsError::UnknownName {
                    line,
                    name: name.to_owned(),
                })?;
            if !seen.insert(name) {
                return Err(SumsError::Duplicate {
                    line,
                    name: name.to_owned(),
                });
            }
            entries.push(SumEntry {
                digest: digest.to_owned(),
                name: name.to_owned(),
                version,
                triple,
            });
        }
        Ok(Self { entries })
    }

    /// The newest version any archive name carries.
    pub(crate) fn latest_version(&self) -> Option<&ReleaseVersion> {
        self.entries
            .iter()
            .map(|entry| &entry.version)
            .max_by(|a, b| a.precedence().cmp(&b.precedence()))
    }

    /// The line for this version and platform, if the release has one.
    pub(crate) fn entry_for(&self, version: &ReleaseVersion, triple: Triple) -> Option<&SumEntry> {
        self.entries
            .iter()
            .find(|entry| entry.triple == triple && entry.version.is_same_release(version))
    }
}

/// Whether `text` is exactly 64 lowercase hex digits.
fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether `name` is non-empty, has no leading space and no path or control characters.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with([' ', '*'])
        && !name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
}

/// The version and platform in `supdoc-X.Y.Z-<triple>.tar.gz`, or `None` for any other name.
fn parse_asset_name(name: &str) -> Option<(ReleaseVersion, Triple)> {
    let stem = name
        .strip_prefix(ARCHIVE_PREFIX)?
        .strip_suffix(ARCHIVE_SUFFIX)?;
    Triple::ALL.iter().find_map(|&triple| {
        let version = stem.strip_suffix(triple.as_str())?.strip_suffix('-')?;
        let version = ReleaseVersion::from_str(version).ok()?;
        Some((version, triple))
    })
}

/// Why no binary came out of a release archive.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ArchiveError {
    /// The bytes are not a readable gzip tar stream.
    #[error("the release archive is not a readable .tar.gz: {0}")]
    Unreadable(String),
    /// An entry path is absolute, climbs with `..`, or is not plain relative text.
    #[error("the release archive has an entry with an unsafe path: `{0}`")]
    UnsafePath(String),
    /// An entry is a link or another special file.
    #[error("the release archive has a link or special entry: `{0}`")]
    SpecialEntry(String),
    /// The expected entry appears more than once.
    #[error("the release archive holds `{0}` more than once")]
    DuplicateBinary(String),
    /// The expected entry exists but is not a regular file.
    #[error("the release archive entry `{0}` is not a regular file")]
    NotRegular(String),
    /// The expected entry is larger than the cap.
    #[error("the release archive entry `{path}` is {size} bytes, over the {limit} byte cap")]
    TooLarge {
        /// The entry path.
        path: String,
        /// The size its header declares.
        size: u64,
        /// The cap.
        limit: u64,
    },
    /// The archive has no entry at the expected path.
    #[error("the release archive has no `{0}` entry")]
    Missing(String),
}

/// The bytes of the one expected binary in a `.tar.gz` held in memory.
///
/// Nothing is unpacked: entries are read in order and only the entry whose path is exactly
/// `supdoc-<version>-<triple>/supdoc[.exe]` is taken. Any entry with an absolute path, a `..`
/// component or a link type refuses the whole archive, as does a second copy of the expected
/// entry, a non-regular one, or one over `max_binary`.
///
/// # Errors
///
/// Returns an [`ArchiveError`] for every refusal above, for a stream that is not readable
/// gzip tar data, and when the expected entry is absent.
pub(crate) fn extract_binary(
    archive: &[u8],
    version: &ReleaseVersion,
    triple: Triple,
    max_binary: u64,
) -> Result<Vec<u8>, ArchiveError> {
    let expected = format!(
        "{ARCHIVE_PREFIX}{version}-{triple}/{}",
        triple.binary_name()
    );
    let decoder = GzDecoder::new(archive).take(max_binary.saturating_add(MAX_EXTRA_BYTES));
    let mut tar = Archive::new(decoder);
    let unreadable = |error: std::io::Error| ArchiveError::Unreadable(error.to_string());
    let mut found: Option<Vec<u8>> = None;
    for entry in tar.entries().map_err(unreadable)? {
        let mut entry = entry.map_err(unreadable)?;
        let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        if !is_safe_path(&path) {
            return Err(ArchiveError::UnsafePath(path));
        }
        let kind = entry.header().entry_type();
        if !matches!(kind, EntryType::Regular | EntryType::Directory) {
            return Err(ArchiveError::SpecialEntry(path));
        }
        if path != expected {
            continue;
        }
        if found.is_some() {
            return Err(ArchiveError::DuplicateBinary(path));
        }
        if kind != EntryType::Regular {
            return Err(ArchiveError::NotRegular(path));
        }
        let size = entry.size();
        if size > max_binary {
            return Err(ArchiveError::TooLarge {
                path,
                size,
                limit: max_binary,
            });
        }
        let mut bytes = Vec::new();
        let _ = entry.read_to_end(&mut bytes).map_err(unreadable)?;
        found = Some(bytes);
    }
    found.ok_or(ArchiveError::Missing(expected))
}

/// Whether `path` is relative, has only normal components and no backslash or NUL.
fn is_safe_path(path: &str) -> bool {
    !path.contains(['\\', '\0'])
        && std::path::Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, Header};

    use super::*;

    fn version(text: &str) -> ReleaseVersion {
        ReleaseVersion::from_str(text).expect("the test version should parse")
    }

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn each_of_the_eight_build_combinations_maps_to_its_triple() {
        let rows = [
            (
                Arch::X86_64,
                Os::Linux,
                Env::Gnu,
                "x86_64-unknown-linux-gnu",
            ),
            (
                Arch::Aarch64,
                Os::Linux,
                Env::Gnu,
                "aarch64-unknown-linux-gnu",
            ),
            (
                Arch::X86_64,
                Os::Linux,
                Env::Musl,
                "x86_64-unknown-linux-musl",
            ),
            (
                Arch::Aarch64,
                Os::Linux,
                Env::Musl,
                "aarch64-unknown-linux-musl",
            ),
            (
                Arch::X86_64,
                Os::Macos,
                Env::Unspecified,
                "x86_64-apple-darwin",
            ),
            (
                Arch::Aarch64,
                Os::Macos,
                Env::Unspecified,
                "aarch64-apple-darwin",
            ),
            (
                Arch::X86_64,
                Os::Windows,
                Env::Msvc,
                "x86_64-pc-windows-msvc",
            ),
            (
                Arch::Aarch64,
                Os::Windows,
                Env::Msvc,
                "aarch64-pc-windows-msvc",
            ),
        ];
        for (arch, os, env, want) in rows {
            let got = triple_for(arch, os, env).map(Triple::as_str);
            assert_eq!(got, Some(want), "{arch:?} {os:?} {env:?}");
        }
    }

    #[test]
    fn combinations_outside_the_eight_have_no_triple() {
        for (arch, os, env) in [
            (Arch::X86_64, Os::Linux, Env::Msvc),
            (Arch::X86_64, Os::Linux, Env::Unspecified),
            (Arch::Aarch64, Os::Macos, Env::Musl),
            (Arch::X86_64, Os::Windows, Env::Gnu),
        ] {
            assert_eq!(triple_for(arch, os, env), None, "{arch:?} {os:?} {env:?}");
        }
    }

    #[test]
    fn the_running_build_is_one_of_the_eight() {
        assert!(
            running_triple().is_some(),
            "tests run on a released platform"
        );
    }

    #[test]
    fn urls_follow_the_asset_contract() {
        let v = version("0.5.0");
        assert_eq!(
            asset_url(&v, Triple::Aarch64Apple),
            "https://github.com/aidenzepp/supdoc/releases/download/v0.5.0/supdoc-0.5.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            sums_url(Some(&v)),
            "https://github.com/aidenzepp/supdoc/releases/download/v0.5.0/SHA256SUMS"
        );
        assert_eq!(
            sums_url(None),
            "https://github.com/aidenzepp/supdoc/releases/latest/download/SHA256SUMS"
        );
    }

    fn line(version: &str, triple: Triple) -> String {
        format!("{DIGEST}  supdoc-{version}-{triple}.tar.gz")
    }

    #[test]
    fn sums_parse_and_name_the_latest_version() {
        let text = format!(
            "{}\n{}\n{}\n",
            line("0.4.0", Triple::X86_64Apple),
            line("0.5.0", Triple::X86_64Apple),
            line("0.5.0-rc.1", Triple::Aarch64LinuxMusl),
        );
        let sums = Sums::parse(&text).expect("a well-formed file should parse");
        assert_eq!(sums.latest_version(), Some(&version("0.5.0")));
        let entry = sums
            .entry_for(&version("0.4.0"), Triple::X86_64Apple)
            .expect("the 0.4.0 line should be found");
        assert_eq!(entry.digest, DIGEST);
        assert_eq!(
            sums.entry_for(&version("0.4.0"), Triple::Aarch64Apple),
            None
        );
    }

    #[test]
    fn sums_lines_that_break_the_format_are_refused() {
        let ok = line("0.5.0", Triple::X86_64Apple);
        let cases = [
            ("empty", String::new(), SumsError::Empty),
            (
                "uppercase digest",
                ok.replace("abcdef0123", "ABCDEF0123"),
                SumsError::Malformed { line: 1 },
            ),
            (
                "one space",
                ok.replace("  ", " "),
                SumsError::Malformed { line: 1 },
            ),
            (
                "binary-mode star",
                ok.replace("  ", " *"),
                SumsError::Malformed { line: 1 },
            ),
            (
                "short digest",
                ok.replacen(DIGEST, &DIGEST.replacen('0', "", 1), 1),
                SumsError::Malformed { line: 1 },
            ),
            (
                "CRLF",
                format!("{ok}\r\n"),
                SumsError::Malformed { line: 1 },
            ),
            (
                "blank line",
                format!("{ok}\n\n{ok}\n"),
                SumsError::Malformed { line: 2 },
            ),
            (
                "unknown name",
                format!("{DIGEST}  LICENSE\n"),
                SumsError::UnknownName {
                    line: 1,
                    name: "LICENSE".into(),
                },
            ),
            (
                "duplicate name",
                format!("{ok}\n{ok}\n"),
                SumsError::Duplicate {
                    line: 2,
                    name: "supdoc-0.5.0-x86_64-apple-darwin.tar.gz".into(),
                },
            ),
        ];
        for (label, text, want) in cases {
            assert_eq!(Sums::parse(&text), Err(want), "{label}");
        }
    }

    /// One archive entry for `archive`: path bytes, type, link target and body.
    struct Item<'a> {
        path: &'a str,
        kind: EntryType,
        link: Option<&'a str>,
        body: &'a [u8],
    }

    fn file<'a>(path: &'a str, body: &'a [u8]) -> Item<'a> {
        Item {
            path,
            kind: EntryType::Regular,
            link: None,
            body,
        }
    }

    fn archive(items: &[Item<'_>]) -> Vec<u8> {
        let mut tar = Builder::new(Vec::new());
        for item in items {
            let mut header = Header::new_gnu();
            let name = &mut header.as_gnu_mut().expect("a gnu header").name;
            name.get_mut(..item.path.len())
                .expect("the path should fit the name field")
                .copy_from_slice(item.path.as_bytes());
            header.set_entry_type(item.kind);
            header.set_size(item.body.len() as u64);
            header.set_mode(0o755);
            if let Some(link) = item.link {
                header
                    .set_link_name(link)
                    .expect("the link name should fit");
            }
            header.set_cksum();
            tar.append(&header, item.body)
                .expect("the entry should append");
        }
        let raw = tar.into_inner().expect("the tar should finish");
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(&raw).expect("gzip should accept the bytes");
        gz.finish().expect("gzip should finish")
    }

    const ENTRY: &str = "supdoc-0.5.0-x86_64-unknown-linux-gnu/supdoc";

    fn extract(bytes: &[u8], cap: u64) -> Result<Vec<u8>, ArchiveError> {
        extract_binary(bytes, &version("0.5.0"), Triple::X86_64LinuxGnu, cap)
    }

    #[test]
    fn the_exact_entry_is_returned_and_siblings_are_ignored() {
        let bytes = archive(&[
            file("supdoc-0.5.0-x86_64-unknown-linux-gnu/LICENSE", b"mit"),
            file(ENTRY, b"binary"),
            file("supdoc-0.5.0-x86_64-unknown-linux-gnu/README.md", b"hi"),
        ]);
        assert_eq!(extract(&bytes, 1024), Ok(b"binary".to_vec()));
    }

    #[test]
    fn windows_archives_take_the_exe() {
        let bytes = archive(&[file(
            "supdoc-0.5.0-x86_64-pc-windows-msvc/supdoc.exe",
            b"exe",
        )]);
        let got = extract_binary(&bytes, &version("0.5.0"), Triple::X86_64Windows, 1024);
        assert_eq!(got, Ok(b"exe".to_vec()));
    }

    #[test]
    fn archives_that_could_escape_or_confuse_yield_no_bytes() {
        let dup = archive(&[file(ENTRY, b"one"), file(ENTRY, b"two")]);
        let parent = archive(&[
            file("supdoc-0.5.0-x86_64-unknown-linux-gnu/../evil", b"x"),
            file(ENTRY, b"ok"),
        ]);
        let absolute = archive(&[file("/etc/passwd", b"x"), file(ENTRY, b"ok")]);
        let symlink = archive(&[
            Item {
                path: "supdoc-0.5.0-x86_64-unknown-linux-gnu/link",
                kind: EntryType::Symlink,
                link: Some("/etc/passwd"),
                body: b"",
            },
            file(ENTRY, b"ok"),
        ]);
        let symlinked_binary = archive(&[Item {
            path: ENTRY,
            kind: EntryType::Symlink,
            link: Some("/bin/sh"),
            body: b"",
        }]);
        let hardlink = archive(&[Item {
            path: ENTRY,
            kind: EntryType::Link,
            link: Some("other"),
            body: b"",
        }]);
        let directory = archive(&[Item {
            path: ENTRY,
            kind: EntryType::Directory,
            link: None,
            body: b"",
        }]);
        let oversize = archive(&[file(ENTRY, &[7; 100])]);
        let wrong_dir = archive(&[file("supdoc-0.4.0-x86_64-unknown-linux-gnu/supdoc", b"x")]);
        let bare = archive(&[file("supdoc", b"x")]);
        let not_gzip = b"plain text".to_vec();
        let cases: [(&str, Vec<u8>, u64); 11] = [
            ("second supdoc entry", dup, 1024),
            ("dotdot", parent, 1024),
            ("absolute", absolute, 1024),
            ("symlink entry", symlink, 1024),
            ("symlinked binary", symlinked_binary, 1024),
            ("hardlinked binary", hardlink, 1024),
            ("directory at the binary path", directory, 1024),
            ("oversize", oversize, 99),
            ("wrong version directory", wrong_dir, 1024),
            ("bare binary name", bare, 1024),
            ("not gzip", not_gzip, 1024),
        ];
        for (label, bytes, cap) in cases {
            assert!(extract(&bytes, cap).is_err(), "{label} must yield no bytes");
        }
    }

    #[test]
    fn the_size_cap_is_inclusive_and_names_the_overrun() {
        let bytes = archive(&[file(ENTRY, &[7; 100])]);
        assert_eq!(extract(&bytes, 100).map(|b| b.len()), Ok(100));
        assert_eq!(extract(&bytes, MAX_BINARY_BYTES).map(|b| b.len()), Ok(100));
        assert_eq!(
            extract(&bytes, 99),
            Err(ArchiveError::TooLarge {
                path: ENTRY.into(),
                size: 100,
                limit: 99,
            })
        );
    }
}
