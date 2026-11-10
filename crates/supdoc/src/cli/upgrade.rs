//! `supdoc upgrade [VERSION]`: installs a newer or older supdoc.
//!
//! This is the only command that uses the network, and only when you run it; nothing checks for
//! updates on its own. A supdoc that cargo installed from crates.io upgrades through cargo: it
//! runs `cargo install supdoc --locked --version =<V> --force` and nothing else, when its own
//! binary is the one cargo installed (`$CARGO_HOME/bin/supdoc`, listed in
//! `$CARGO_HOME/.crates2.json`). Any other install is refused with the right manual step. Cargo
//! runs from `$CARGO_HOME`, so no `.cargo/config.toml` or `rust-toolchain.toml` near the
//! directory supdoc was started in can redirect it. Without `--yes` the command only previews.
//! With no version, `cargo search supdoc --limit 1` names the latest release.
//!
//! A binary built by the release workflow (it carries `SUPDOC_RELEASE_BINARY=1`) is the other
//! install this command knows. Its preview reads one `SHA256SUMS` from the GitHub release and
//! names the archive and digest that would be installed. Under `--yes` it fetches that archive,
//! checks its SHA-256 against `SHA256SUMS`, and only then replaces itself, atomically, beside
//! the old file.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read as _, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;

use schemars::JsonSchema;
use serde::Serialize;

use crate::exit::ExitStatus;
use crate::lock::error::shell_quote;
use crate::output::json::Envelope;
use crate::output::text::visible;
use crate::output::{Output, Report, TextContext, preview_note};

use super::args::{UpgradeArgs, WriteMode};
use super::project::command_line;
use super::{CliError, Context, Outcome, cause};
use release::{Sums, Triple};

mod fetch;
mod install;
mod release;

/// Shown after `supdoc upgrade --help`.
pub(crate) const UPGRADE_EXAMPLES: &str = "\
Examples:
  # Preview installing the latest release
  supdoc upgrade

  # Install the latest release
  supdoc upgrade --yes

  # Downgrade to an exact version
  supdoc upgrade 0.3.0 --yes

  # Machine-readable preview
  supdoc upgrade 0.3.0 --json";

/// The package cargo installs.
const PACKAGE: &str = "supdoc";

/// The shape of cargo's error when the pinned version is not on crates.io.
const NOT_FOUND_SHAPE: &str = "could not find `supdoc` in registry";

/// The source cargo records for a package installed from crates.io, over the git index.
const CRATES_IO_GIT: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// The source cargo records for a package installed from crates.io, over the sparse index.
const CRATES_IO_SPARSE: &str = "registry+sparse+https://index.crates.io";

/// A release version: `MAJOR.MINOR.PATCH[-PRE][+BUILD]`, checked by hand.
///
/// A leading `v` is refused, so the version is the exact text cargo takes after `=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReleaseVersion {
    /// The version as typed, build metadata included.
    text: String,
    /// The major number.
    major: u64,
    /// The minor number.
    minor: u64,
    /// The patch number.
    patch: u64,
    /// The dot-separated pre-release identifiers, empty for a release.
    pre: Vec<PreIdentifier>,
}

/// One dot-separated identifier of a pre-release, ordered as semver orders them: numbers before
/// words, numbers by value, words by bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PreIdentifier {
    /// Digits only, without a leading zero.
    Numeric(u64),
    /// Anything else made of letters, digits and hyphens.
    Word(String),
}

impl ReleaseVersion {
    /// The version without build metadata, which is what cargo pins and what compares.
    fn release(&self) -> &str {
        self.text.split('+').next().unwrap_or(&self.text)
    }

    /// Whether `other` is the same release: build metadata does not count.
    fn is_same_release(&self, other: &Self) -> bool {
        self.precedence() == other.precedence()
    }

    /// The parts semver orders by.
    fn precedence(&self) -> (u64, u64, u64, PreRelease<'_>) {
        (self.major, self.minor, self.patch, PreRelease(&self.pre))
    }

    /// Whether moving from `self` to `target` upgrades, downgrades or stays.
    fn direction_to(&self, target: &Self) -> Direction {
        match self.precedence().cmp(&target.precedence()) {
            std::cmp::Ordering::Less => Direction::Upgrade,
            std::cmp::Ordering::Equal => Direction::Same,
            std::cmp::Ordering::Greater => Direction::Downgrade,
        }
    }
}

/// A pre-release list that sorts as semver says: a release is newer than any of its
/// pre-releases, and otherwise identifiers compare left to right, the shorter list first.
#[derive(Debug, PartialEq, Eq)]
struct PreRelease<'a>(&'a [PreIdentifier]);

impl PartialOrd for PreRelease<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PreRelease<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self.0.is_empty(), other.0.is_empty()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => self.0.cmp(other.0),
        }
    }
}

impl fmt::Display for ReleaseVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl FromStr for ReleaseVersion {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if let Some(rest) = text.strip_prefix(['v', 'V'])
            && Self::from_str(rest).is_ok()
        {
            return Err(format!("write `{rest}`, not `{text}`"));
        }
        parse_release(text).ok_or_else(|| {
            format!(
                "`{text}` is not a version: write MAJOR.MINOR.PATCH such as `0.3.1`, optionally \
                 with a pre-release such as `0.4.0-rc.1` or build metadata such as `0.3.1+build`"
            )
        })
    }
}

/// `text` as a release version, or `None` when it is not `MAJOR.MINOR.PATCH[-PRE][+BUILD]`.
fn parse_release(text: &str) -> Option<ReleaseVersion> {
    let (without_build, build) = match text.split_once('+') {
        Some((head, build)) => (head, Some(build)),
        None => (text, None),
    };
    if build.is_some_and(|build| !build.split('.').all(is_identifier)) {
        return None;
    }
    let (numbers, pre) = match without_build.split_once('-') {
        Some((numbers, pre)) => (numbers, Some(pre)),
        None => (without_build, None),
    };
    let mut parts = numbers.split('.');
    let major = parse_number(parts.next()?)?;
    let minor = parse_number(parts.next()?)?;
    let patch = parse_number(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    let pre = match pre {
        None => Vec::new(),
        Some(pre) => pre
            .split('.')
            .map(parse_pre_identifier)
            .collect::<Option<Vec<_>>>()?,
    };
    Some(ReleaseVersion {
        text: text.to_owned(),
        major,
        minor,
        patch,
        pre,
    })
}

/// Whether `text` is a non-empty run of ASCII letters, digits and hyphens.
fn is_identifier(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// A version number: digits only, with no leading zero.
fn parse_number(text: &str) -> Option<u64> {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    text.parse().ok()
}

/// One pre-release identifier: a number without a leading zero, or a word.
fn parse_pre_identifier(text: &str) -> Option<PreIdentifier> {
    if !is_identifier(text) {
        None
    } else if text.bytes().all(|byte| byte.is_ascii_digit()) {
        parse_number(text).map(PreIdentifier::Numeric)
    } else {
        Some(PreIdentifier::Word(text.to_owned()))
    }
}

/// Which way the install moves relative to the version that is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Direction {
    /// The target is newer.
    Upgrade,
    /// The target is older.
    Downgrade,
    /// The target is the running version.
    Same,
}

/// How the run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
enum UpgradeOutcome {
    /// Nothing was run; this is the command that would be.
    Previewed,
    /// The target version was installed.
    Installed,
    /// The target is the running version, so nothing ran.
    AlreadyCurrent,
}

/// How this supdoc was installed, and so how it would be upgraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
enum Method {
    /// `cargo install` from crates.io.
    Cargo,
    /// A binary downloaded from a GitHub release.
    ReleaseBinary,
}

/// `supdoc upgrade`: the versions, and what installs the target.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct UpgradeReport {
    /// The version that is running.
    current: String,
    /// The version to install.
    target: String,
    /// `upgrade`, `downgrade`, or `same` when the target is the running version.
    direction: Direction,
    /// The command that installs the target, as an argument vector; empty when nothing would run
    /// and for a release binary.
    command: Vec<String>,
    /// Whether the upgrade ran, by cargo or by replacing the binary.
    ran: bool,
    /// How the run ended.
    outcome: UpgradeOutcome,
    /// How this supdoc was installed: `cargo` or `release-binary`.
    method: Method,
    /// The release archive URL the target would come from; only for a release binary that has a
    /// version to install.
    #[serde(skip_serializing_if = "Option::is_none")]
    asset: Option<String>,
    /// The SHA-256 the archive must have, from `SHA256SUMS`; only with `asset`.
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

impl Report for UpgradeReport {
    fn command(&self) -> &'static str {
        "upgrade"
    }

    fn render_text(&self, _context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        if self.outcome == UpgradeOutcome::AlreadyCurrent {
            return writeln!(out, "already on {}", visible(&self.current));
        }
        let verb = match self.direction {
            Direction::Downgrade => "downgrade",
            Direction::Upgrade | Direction::Same => "upgrade",
        };
        writeln!(out, "current  {}", visible(&self.current))?;
        writeln!(out, "target   {} ({verb})", visible(&self.target))?;
        match (&self.asset, &self.sha256) {
            (Some(asset), Some(sha256)) => {
                writeln!(out, "asset    {}", visible(asset))?;
                writeln!(out, "sha256   {}", visible(sha256))
            }
            _ => writeln!(out, "command  {}", shell_line(&self.command)),
        }
    }
}

/// `argv` as one shell command line.
fn shell_line(argv: &[String]) -> String {
    argv.iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The JSON Schema of `supdoc upgrade`'s `--json` output: the whole envelope, with `data` typed
/// as [`UpgradeReport`].
#[must_use]
pub(crate) fn schema() -> schemars::Schema {
    schemars::schema_for!(Envelope<'static, UpgradeReport>)
}

/// Runs `supdoc upgrade`: previews, or under `--yes` runs, the install of the target.
///
/// A supdoc that cargo installed from crates.io upgrades with cargo. A release binary previews
/// the archive and digest from the release's `SHA256SUMS`.
///
/// Effect: reads `CARGO_HOME`, `HOME`, the install records and this binary's path. The cargo
/// route runs `cargo search` when no version is given, and `cargo install` under `--yes`, both
/// with `$CARGO_HOME` as the working directory and standard input closed. The release-binary
/// route fetches one `SHA256SUMS`.
///
/// # Errors
///
/// Returns `cli-upgrade-not-cargo` (78) when this binary is neither a release binary nor the one
/// cargo installed from crates.io, `cli-upgrade-no-cargo` (74) when `cargo` cannot be started,
/// `cli-upgrade-cargo-failed` (1) when cargo exits with a failure, `cli-upgrade-latest-unknown`
/// (1) when `cargo search` names no version, `cli-upgrade-fetch-failed` (74) when `SHA256SUMS`
/// cannot be fetched or read, `cli-upgrade-no-asset` (1) when the release has no archive for
/// this platform, `cli-upgrade-verify-failed` (1) when the downloaded archive does not match
/// its checksum or lacks the expected binary, `cli-upgrade-replace-failed` (74) when the binary
/// cannot be replaced, and `cli-output-failed` when the report cannot be written.
pub(crate) fn run(
    args: &UpgradeArgs,
    context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    let environment = Environment::read();
    match detect(&environment, release_marker())? {
        Install::Cargo(installed) => run_cargo(args, context, output, &installed),
        Install::ReleaseBinary => {
            run_release_binary(args, context, output, environment.exe.as_deref())
        }
    }
}

/// How this supdoc was installed.
#[derive(Debug)]
enum Install {
    /// By cargo from crates.io, with cargo's record to prove it.
    Cargo(CargoInstall),
    /// As a binary downloaded from a release.
    ReleaseBinary,
}

/// Whether this build says it is a release binary: the release workflow sets
/// `SUPDOC_RELEASE_BINARY=1` at compile time, and debug builds also take a
/// `SUPDOC_TEST_RELEASE_DIR`, so tests can stand in for the release.
fn release_marker() -> bool {
    option_env!("SUPDOC_RELEASE_BINARY") == Some("1") || fetch::test_dir_is_set()
}

/// Decides how this supdoc was installed. Order: the binary is the one cargo recorded (the cargo
/// route, checked as before); a location another tool owns (Homebrew, a `target/` build), which
/// is refused even with the release marker, since supdoc never takes over a file another tool
/// owns; the release marker; then the unknown-location refusal of [`installed_by_cargo`].
///
/// # Errors
///
/// Returns `cli-upgrade-not-cargo` as [`installed_by_cargo`] does, for a known owner or, with no
/// marker, an unknown origin.
fn detect(environment: &Environment, marker: bool) -> Result<Install, CliError> {
    if cargo_owns_exe(environment) {
        return installed_by_cargo(environment).map(Install::Cargo);
    }
    if let Some(refusal) = environment.exe.as_deref().and_then(known_location) {
        return Err(refusal);
    }
    if marker {
        return Ok(Install::ReleaseBinary);
    }
    installed_by_cargo(environment).map(Install::Cargo)
}

/// Whether this binary is the file at `$CARGO_HOME/bin/supdoc`.
fn cargo_owns_exe(environment: &Environment) -> bool {
    let (Some(exe), Ok(cargo_home)) = (&environment.exe, &environment.cargo_home) else {
        return false;
    };
    fs::canonicalize(cargo_home.join("bin").join(PACKAGE))
        .ok()
        .as_deref()
        == Some(exe.as_path())
}

/// The cargo route of [`run`]: previews, or under `--yes` runs, the cargo install of the target.
///
/// # Errors
///
/// Returns the cargo errors [`run`] lists, and `cli-output-failed` when the report cannot be
/// written.
fn run_cargo(
    args: &UpgradeArgs,
    context: &Context,
    output: &mut Output,
    installed: &CargoInstall,
) -> Result<Outcome, CliError> {
    let target = match &args.release {
        Some(version) => version.clone(),
        None => latest(&installed.cargo_home)?,
    };
    let direction = installed.version.direction_to(&target);
    let applied = args.confirm.mode() == WriteMode::Apply;

    let (command, ran, outcome) = if direction == Direction::Same {
        (Vec::new(), false, UpgradeOutcome::AlreadyCurrent)
    } else {
        let words = install_argv(&target, &installed.cargo_home);
        if applied {
            install(
                &words,
                &installed.cargo_home,
                context.global.format().is_json(),
            )?;
            (words, true, UpgradeOutcome::Installed)
        } else {
            (words, false, UpgradeOutcome::Previewed)
        }
    };

    let report = UpgradeReport {
        current: installed.version.to_string(),
        target: target.to_string(),
        direction,
        command,
        ran,
        outcome,
        method: Method::Cargo,
        asset: None,
        sha256: None,
    };
    output
        .report(&report, true)
        .map_err(|error| CliError::output_failed("the upgrade report", &error))?;
    let note = match outcome {
        UpgradeOutcome::AlreadyCurrent => None,
        UpgradeOutcome::Installed => Some(format!("Installed supdoc {target}.")),
        UpgradeOutcome::Previewed => Some(preview_note(
            args.confirm.dry_run,
            &command_line(context, "upgrade", &[target.release()], &["--yes"]),
        )),
    };
    if let Some(note) = note {
        output
            .note(&note)
            .map_err(|error| CliError::output_failed("the upgrade report", &error))?;
    }
    Ok(Outcome::Clean)
}

/// The first release that has downloadable binaries.
const FIRST_BINARY_RELEASE: &str = "0.5.0";

/// The release-binary route of [`run`]: reads `SHA256SUMS` and previews the archive and digest;
/// under `--yes` fetches the archive, verifies it and replaces the binary at `exe`.
///
/// Effect: one fetch of `SHA256SUMS`, for the latest release or for the version asked for; under
/// `--yes` one more of the archive, then a file created, renamed over `exe` and removed beside
/// it.
///
/// # Errors
///
/// Returns `cli-upgrade-no-asset` for a platform, version or release without an archive,
/// `cli-upgrade-fetch-failed` when `SHA256SUMS` cannot be fetched or parsed,
/// `cli-upgrade-fetch-failed` also when the archive cannot be fetched under `--yes`,
/// `cli-upgrade-verify-failed` and `cli-upgrade-replace-failed` as [`run`] describes, and
/// `cli-output-failed` when the report cannot be written.
fn run_release_binary(
    args: &UpgradeArgs,
    context: &Context,
    output: &mut Output,
    exe: Option<&Path>,
) -> Result<Outcome, CliError> {
    let running = ReleaseVersion::from_str(env!("CARGO_PKG_VERSION"))
        .map_err(|_| cannot_tell("this binary's own version is not a release version"))?;
    if let Some(version) = args
        .release
        .as_ref()
        .filter(|version| version.to_string().contains('+'))
    {
        return Err(no_asset(
            format!("`{version}` carries build metadata, which does not name a release"),
            format!("Write the version as `{}`", version.release()),
        ));
    }
    let same = args
        .release
        .as_ref()
        .is_some_and(|version| running.direction_to(version) == Direction::Same);
    let (target, found) = match args.release.as_ref().filter(|_| same) {
        Some(version) => (version.clone(), None),
        None => find_release_entry(args.release.as_ref())?,
    };
    let direction = running.direction_to(&target);
    let applied = args.confirm.mode() == WriteMode::Apply;

    let (outcome, asset, sha256) = match found {
        Some((asset, digest)) if direction != Direction::Same => {
            if applied {
                for note in install_release(&target, exe, &asset, &digest, context)? {
                    output
                        .note(&note)
                        .map_err(|error| CliError::output_failed("the upgrade report", &error))?;
                }
                (UpgradeOutcome::Installed, Some(asset), Some(digest))
            } else {
                (UpgradeOutcome::Previewed, Some(asset), Some(digest))
            }
        }
        _ => (UpgradeOutcome::AlreadyCurrent, None, None),
    };

    let report = UpgradeReport {
        current: running.to_string(),
        target: target.to_string(),
        direction,
        command: Vec::new(),
        ran: outcome == UpgradeOutcome::Installed,
        outcome,
        method: Method::ReleaseBinary,
        asset,
        sha256,
    };
    output
        .report(&report, true)
        .map_err(|error| CliError::output_failed("the upgrade report", &error))?;
    let note = match outcome {
        UpgradeOutcome::Previewed => Some(preview_note(
            args.confirm.dry_run,
            &command_line(context, "upgrade", &[target.release()], &["--yes"]),
        )),
        UpgradeOutcome::Installed => Some(format!("Installed supdoc {target}.")),
        UpgradeOutcome::AlreadyCurrent => None,
    };
    if let Some(note) = note {
        output
            .note(&note)
            .map_err(|error| CliError::output_failed("the upgrade report", &error))?;
    }
    Ok(Outcome::Clean)
}

/// Fetches the archive at `asset`, checks it against `digest`, and replaces `exe` with its
/// binary.
///
/// Returns the notes about files an earlier upgrade left that could not be removed.
///
/// Effect: one fetch of the archive, the removals of [`install::remove_leftovers`], then the
/// file changes of [`install::replace_exe`].
///
/// # Errors
///
/// Returns `cli-upgrade-fetch-failed` when the archive cannot be fetched,
/// `cli-upgrade-verify-failed` when it fails the checksum or lacks the binary, and
/// `cli-upgrade-replace-failed` when the binary cannot be put in place.
fn install_release(
    target: &ReleaseVersion,
    exe: Option<&Path>,
    asset: &str,
    digest: &str,
    context: &Context,
) -> Result<Vec<String>, CliError> {
    let retry = command_line(context, "upgrade", &[target.release()], &["--yes"]);
    let exe = exe.ok_or_else(|| cannot_tell("its own path could not be found"))?;
    let triple = release::running_triple().ok_or_else(no_asset_for_platform)?;
    let archive = fetch::fetch(asset, fetch::MAX_ARCHIVE_BYTES).map_err(|error| {
        CliError::new(
            "cli-upgrade-fetch-failed",
            format!(
                "could not download the release archive at {}: {error}",
                visible(asset)
            ),
            format!("Check your network, then run `{retry}` again; nothing was changed"),
            ExitStatus::Io,
        )
    })?;
    let binary = install::verified_binary(&archive, digest, target, triple).map_err(|error| {
        CliError::new(
            "cli-upgrade-verify-failed",
            format!("refusing to install {}: {error}", visible(asset)),
            format!(
                "The archive was not trusted, so nothing was changed; run `{retry}` again, or \
                 install with cargo: `cargo install supdoc --locked`"
            ),
            ExitStatus::Failed,
        )
    })?;
    let notes = install::remove_leftovers(exe);
    install::replace_exe(exe, &binary).map_err(|error| {
        CliError::new(
            "cli-upgrade-replace-failed",
            format!(
                "could not replace {}: {error}",
                visible(&exe.display().to_string())
            ),
            format!(
                "Make the binary's directory writable for your user and run `{retry}` again; \
                 the old binary was left in place"
            ),
            ExitStatus::Io,
        )
    })?;
    Ok(notes)
}

/// The version to install and, when it differs from the running one, the archive URL and digest
/// for this platform, from one fetch of `SHA256SUMS`.
///
/// Effect: one fetch of `SHA256SUMS`, for the latest release or for `requested`.
///
/// # Errors
///
/// Returns `cli-upgrade-no-asset` for a platform, version or release without an archive, and
/// `cli-upgrade-fetch-failed` when `SHA256SUMS` cannot be fetched or parsed.
fn find_release_entry(
    requested: Option<&ReleaseVersion>,
) -> Result<(ReleaseVersion, Option<(String, String)>), CliError> {
    let triple = release::running_triple().ok_or_else(no_asset_for_platform)?;
    let url = release::sums_url(requested).ok_or_else(|| {
        no_asset(
            format!(
                "{} is a pre-release, and releases publish binaries only for plain versions",
                requested.map_or_else(String::new, ToString::to_string)
            ),
            "Name a plain version such as `0.5.0`, or install the pre-release with cargo",
        )
    })?;
    let bytes = fetch::fetch(&url, fetch::MAX_SUMS_BYTES)
        .map_err(|error| sums_fetch_error(&url, requested, error))?;
    let text =
        String::from_utf8(bytes).map_err(|_| fetch_failed(&url, &"the file is not UTF-8"))?;
    let sums = Sums::parse(&text).map_err(|error| fetch_failed(&url, &error))?;
    let target = match requested {
        Some(version) => version.clone(),
        None => sums
            .latest_version()
            .cloned()
            .ok_or_else(|| fetch_failed(&url, &"it lists no version"))?,
    };
    let running = ReleaseVersion::from_str(env!("CARGO_PKG_VERSION"))
        .map_err(|_| cannot_tell("this binary's own version is not a release version"))?;
    if running.direction_to(&target) == Direction::Same {
        return Ok((target, None));
    }
    let entry = sums
        .entry_for(&target, triple)
        .ok_or_else(|| no_asset_for(&target, triple))?;
    let asset = release::asset_url(&target, triple).ok_or_else(|| no_asset_for(&target, triple))?;
    Ok((target, Some((asset, entry.digest.clone()))))
}

/// The refusal for a platform with no release binary.
fn no_asset_for_platform() -> CliError {
    no_asset(
        format!(
            "supdoc has no release binary for this platform ({}-{})",
            std::env::consts::ARCH,
            std::env::consts::OS
        ),
        "Install it with cargo: `cargo install supdoc --locked`",
    )
}

/// The error for a failed fetch of `SHA256SUMS`: a 404 means the release does not exist, so
/// it is `cli-upgrade-no-asset`; every other failure is `cli-upgrade-fetch-failed`.
fn sums_fetch_error(
    url: &str,
    requested: Option<&ReleaseVersion>,
    error: fetch::FetchError,
) -> CliError {
    match error {
        fetch::FetchError::Status { status: 404, .. } => no_sums(url, requested),
        other => fetch_failed(url, &other),
    }
}

/// The refusal for a release whose `SHA256SUMS` does not exist (the server answered 404).
fn no_sums(url: &str, requested: Option<&ReleaseVersion>) -> CliError {
    let which = requested.map_or_else(
        || "the latest release".to_owned(),
        |v| format!("release {v}"),
    );
    no_asset(
        format!(
            "{which} has no SHA256SUMS at {} (the server answered 404), so it has no binaries",
            visible(url)
        ),
        manual_route(requested),
    )
}

/// The manual way to get a version that has no binary for this platform.
fn manual_route(version: Option<&ReleaseVersion>) -> String {
    let pin = version.map_or_else(String::new, |v| format!(" --version ={}", v.release()));
    format!(
        "Download an archive by hand from the supdoc releases page on GitHub, or install with cargo: `cargo install supdoc --locked{pin}`"
    )
}

/// A `cli-upgrade-no-asset` refusal, exit 1.
fn no_asset(message: String, help: impl Into<String>) -> CliError {
    CliError::new("cli-upgrade-no-asset", message, help, ExitStatus::Failed)
}

/// The refusal for a release that has no archive for `triple`.
fn no_asset_for(target: &ReleaseVersion, triple: Triple) -> CliError {
    let before_binaries = ReleaseVersion::from_str(FIRST_BINARY_RELEASE)
        .is_ok_and(|first| first.direction_to(target) == Direction::Downgrade);
    let manual = manual_route(Some(target));
    let help = if before_binaries {
        format!(
            "Releases before {FIRST_BINARY_RELEASE} have no binaries; run `supdoc upgrade \
             {FIRST_BINARY_RELEASE}` for the first one. {manual}"
        )
    } else {
        manual
    };
    no_asset(
        format!(
            "release {target} lists no binary for {triple}",
            triple = triple.as_str()
        ),
        help,
    )
}

/// The error for a `SHA256SUMS` that could not be fetched or read.
fn fetch_failed(url: &str, reason: &dyn fmt::Display) -> CliError {
    CliError::new(
        "cli-upgrade-fetch-failed",
        format!(
            "could not read the release checksums at {}: {reason}",
            visible(url)
        ),
        "Check your network and that the version has a release, then run `supdoc upgrade` again",
        ExitStatus::Io,
    )
}

/// The command that installs `target`: `=` pins the exact version, so cargo never picks a
/// compatible newer one, `--locked` keeps the grammars the queries were written for, `--root`
/// installs into the cargo home that was checked, wherever `CARGO_INSTALL_ROOT` or cargo's
/// `install.root` would send it, and `--color never` keeps cargo's text plain.
fn install_argv(target: &ReleaseVersion, cargo_home: &Path) -> Vec<String> {
    [
        "cargo",
        "install",
        PACKAGE,
        "--locked",
        "--version",
        &format!("={}", target.release()),
        "--force",
        "--root",
        cargo_home.to_str().unwrap_or_default(),
        "--color",
        "never",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// What supdoc reads from its surroundings to decide how it was installed.
#[derive(Debug)]
struct Environment {
    /// This binary's path with symlinks resolved, when it can be found.
    exe: Option<PathBuf>,
    /// The cargo home: `CARGO_HOME` when absolute, else `$HOME/.cargo`, or why there is none.
    cargo_home: Result<PathBuf, CargoHomeProblem>,
}

/// Why no usable cargo home was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CargoHomeProblem {
    /// `CARGO_HOME` is set to a relative path.
    RelativeSet,
    /// `CARGO_HOME` is unset or empty, and `HOME` is not an absolute path.
    Unset,
    /// The cargo home is not valid UTF-8, so it cannot be shown or passed to cargo faithfully.
    NotUtf8,
}

/// The cargo home that `CARGO_HOME` and `HOME` name, or why they name none.
///
/// An unset or empty `CARGO_HOME` falls back to `$HOME/.cargo`. A relative `CARGO_HOME` or `HOME`
/// is no cargo home, since cargo would read it from wherever supdoc happens to run.
///
/// # Errors
///
/// Returns the [`CargoHomeProblem`]: a relative or missing path, or one that is not UTF-8.
fn cargo_home_from(
    cargo_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, CargoHomeProblem> {
    let found = if let Some(value) = cargo_home.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err(CargoHomeProblem::RelativeSet);
        }
        path
    } else {
        home.filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or(CargoHomeProblem::Unset)?
            .join(".cargo")
    };
    if found.to_str().is_some() {
        Ok(found)
    } else {
        Err(CargoHomeProblem::NotUtf8)
    }
}

impl Environment {
    /// Reads the current executable, `CARGO_HOME` and `HOME`.
    fn read() -> Self {
        Self::from_parts(
            std::env::current_exe().ok(),
            std::env::var_os("CARGO_HOME"),
            std::env::var_os("HOME"),
        )
    }

    /// The environment for an executable path and the two variables, as [`cargo_home_from`] reads
    /// them.
    fn from_parts(
        exe: Option<PathBuf>,
        cargo_home: Option<OsString>,
        home: Option<OsString>,
    ) -> Self {
        let cargo_home = cargo_home_from(cargo_home, home);
        Self {
            exe: exe.and_then(|exe| fs::canonicalize(exe).ok()),
            cargo_home,
        }
    }
}

/// A supdoc that cargo installed from crates.io.
#[derive(Debug)]
struct CargoInstall {
    /// The installed version, from cargo's record.
    version: ReleaseVersion,
    /// The cargo home cargo runs from.
    cargo_home: PathBuf,
}

/// Decides how this supdoc was installed, refusing anything but a cargo install from crates.io.
///
/// Effect: reads `$CARGO_HOME/bin/supdoc` and `$CARGO_HOME/.crates2.json`.
///
/// # Errors
///
/// Returns `cli-upgrade-not-cargo` naming what it saw and the manual step for it.
fn installed_by_cargo(environment: &Environment) -> Result<CargoInstall, CliError> {
    let seen = environment.exe.as_deref().map_or_else(
        || "an unknown location".to_owned(),
        |exe| exe.display().to_string(),
    );
    let (Some(exe), Ok(cargo_home)) = (&environment.exe, &environment.cargo_home) else {
        return Err(match (&environment.exe, &environment.cargo_home) {
            (Some(exe), Err(problem)) => {
                known_location(exe).unwrap_or_else(|| no_cargo_home(*problem))
            }
            _ => cannot_tell(&seen),
        });
    };
    let expected = fs::canonicalize(cargo_home.join("bin").join(PACKAGE)).ok();
    if expected.as_deref() != Some(exe.as_path()) {
        return Err(by_location(exe));
    }
    let record = fs::read_to_string(cargo_home.join(".crates2.json")).map_err(|error| {
        cannot_tell_with(
            &seen,
            &format!(
                "cargo's install record could not be read: {}",
                cause(&error)
            ),
        )
    })?;
    let version = recorded_version(&record, &seen)?;
    Ok(CargoInstall {
        version,
        cargo_home: cargo_home.clone(),
    })
}

/// The refusal for a binary outside `$CARGO_HOME/bin`: Homebrew, a build, or unknown.
fn by_location(exe: &Path) -> CliError {
    known_location(exe).unwrap_or_else(|| cannot_tell(&exe.display().to_string()))
}

/// The refusal for a binary that is plainly a Homebrew install or a build from source, if it is.
fn known_location(exe: &Path) -> Option<CliError> {
    let shown = exe.display().to_string();
    let homebrew = exe.components().any(|part| part.as_os_str() == "Cellar")
        || exe.starts_with("/opt/homebrew")
        || exe.starts_with("/home/linuxbrew/.linuxbrew");
    let build = exe
        .components()
        .any(|part| matches!(part, Component::Normal(name) if name == "target"));
    if homebrew {
        Some(not_cargo(
            format!("supdoc was installed by Homebrew ({shown}), not by cargo"),
            "Run `brew upgrade supdoc`",
        ))
    } else if build {
        Some(not_cargo(
            format!("supdoc is a build from source ({shown}), not a cargo install"),
            "Pull and rebuild it, or install the release with `cargo install supdoc --locked`",
        ))
    } else {
        None
    }
}

/// The refusal when no usable cargo home can be found, so cargo's install records cannot be read.
fn no_cargo_home(problem: CargoHomeProblem) -> CliError {
    let (message, help) = match problem {
        CargoHomeProblem::RelativeSet => (
            "CARGO_HOME is set but is not an absolute path, so supdoc can't find cargo's install \
             records",
            "Set CARGO_HOME to an absolute path, such as `export CARGO_HOME=$HOME/.cargo`, then \
             run `supdoc upgrade` again",
        ),
        CargoHomeProblem::Unset => (
            "CARGO_HOME is not set and HOME is not an absolute path, so supdoc can't find \
             cargo's install records",
            "Set CARGO_HOME to an absolute path, such as `export CARGO_HOME=$HOME/.cargo`, then \
             run `supdoc upgrade` again",
        ),
        CargoHomeProblem::NotUtf8 => (
            "CARGO_HOME (or the HOME it comes from) is not valid UTF-8, so supdoc can't pass it \
             to cargo",
            "Move cargo's home to a UTF-8 path and set CARGO_HOME to it, such as `export \
             CARGO_HOME=$HOME/.cargo`, then run `supdoc upgrade` again",
        ),
    };
    not_cargo(message.to_owned(), help)
}

/// The refusal for a binary whose origin supdoc cannot establish.
fn cannot_tell(shown: &str) -> CliError {
    not_cargo(
        format!("supdoc can't tell how this copy ({shown}) was installed"),
        "Reinstall it with `cargo install supdoc --locked`, then run `supdoc upgrade`",
    )
}

/// [`cannot_tell`] with the reason it could not tell.
fn cannot_tell_with(shown: &str, reason: &str) -> CliError {
    not_cargo(
        format!("supdoc can't tell how this copy ({shown}) was installed: {reason}"),
        "Reinstall it with `cargo install supdoc --locked`, then run `supdoc upgrade`",
    )
}

/// A `cli-upgrade-not-cargo` refusal, exit 78.
fn not_cargo(message: String, help: &str) -> CliError {
    CliError::new("cli-upgrade-not-cargo", message, help, ExitStatus::Config)
}

/// The version cargo recorded for the `supdoc` binary in `record` (`.crates2.json`), checked
/// against this binary's own version.
///
/// # Errors
///
/// Returns `cli-upgrade-not-cargo` when the record is not JSON, holds no `supdoc` that installed
/// the `supdoc` binary, holds more than one, names a source other than crates.io, or names a
/// version other than the one running.
fn recorded_version(record: &str, seen: &str) -> Result<ReleaseVersion, CliError> {
    let document: serde_json::Value = serde_json::from_str(record).map_err(|error| {
        cannot_tell_with(
            seen,
            &format!("cargo's install record is not JSON: {error}"),
        )
    })?;
    let mut entries = document
        .get("installs")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(key, install)| {
            let (name, rest) = key.split_once(' ')?;
            let (version, source) = rest.split_once(' ')?;
            let source = source.strip_prefix('(')?.strip_suffix(')')?;
            let installs_binary = install
                .get("bins")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|bins| bins.iter().any(|bin| bin.as_str() == Some(PACKAGE)));
            (name == PACKAGE && installs_binary).then(|| (version.to_owned(), source.to_owned()))
        });
    let (Some((version, source)), None) = (entries.next(), entries.next()) else {
        return Err(cannot_tell_with(
            seen,
            "cargo's install record does not list exactly one `supdoc` package that installed the `supdoc` binary",
        ));
    };
    let from_crates_io = [CRATES_IO_GIT, CRATES_IO_SPARSE]
        .iter()
        .any(|known| source.trim_end_matches('/') == *known);
    if !from_crates_io {
        return Err(not_cargo(
            format!(
                "supdoc was installed by cargo from {}, not from crates.io",
                shell_quote(&source)
            ),
            "Reinstall it the same way, or install the release with `cargo install supdoc --locked`",
        ));
    }
    let recorded = ReleaseVersion::from_str(&version).map_err(|_| {
        cannot_tell_with(
            seen,
            &format!("cargo's install record names the version `{version}`"),
        )
    })?;
    let running = ReleaseVersion::from_str(env!("CARGO_PKG_VERSION")).map_err(|_| {
        cannot_tell_with(seen, "this binary's own version is not a release version")
    })?;
    if !recorded.is_same_release(&running) {
        return Err(cannot_tell_with(
            seen,
            &format!("cargo's install record says {recorded} but this binary is {running}"),
        ));
    }
    Ok(recorded)
}

/// The latest release on crates.io, from `cargo search supdoc --limit 1`.
///
/// Effect: runs `cargo search`, which reads the network.
///
/// # Errors
///
/// Returns `cli-upgrade-no-cargo` when cargo cannot start, `cli-upgrade-cargo-failed` when it
/// fails, and `cli-upgrade-latest-unknown` when its output has no `supdoc = "<version>"` line.
fn latest(cargo_home: &Path) -> Result<ReleaseVersion, CliError> {
    let argv: Vec<String> = [
        "cargo", "search", PACKAGE, "--limit", "1", "--color", "never",
    ]
    .map(str::to_owned)
    .to_vec();
    let result = cargo(&argv)
        .current_dir(cargo_home)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| no_cargo(&error))?;
    let stdout = String::from_utf8_lossy(&result.stdout).into_owned();
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        // Writing cargo's own message is best effort: the error that follows names the failure.
        let _written = io::stderr().write_all(stderr.as_bytes());
        return Err(cargo_failed(&argv, result.status, &stderr));
    }
    parse_search(&stdout).ok_or_else(|| {
        CliError::new(
            "cli-upgrade-latest-unknown",
            format!(
                "`{}` printed no `supdoc = \"VERSION\"` line, so the latest version is unknown; \
                 it printed: {}",
                shell_line(&argv),
                visible(stdout.trim())
            ),
            "Name the version yourself, such as `supdoc upgrade 0.3.1`",
            ExitStatus::Failed,
        )
    })
}

/// The version on the `supdoc = "<V>"` line of `cargo search` output.
fn parse_search(stdout: &str) -> Option<ReleaseVersion> {
    stdout.lines().find_map(|line| {
        let rest = line.strip_prefix(PACKAGE)?.trim_start().strip_prefix('=')?;
        let version = rest.trim_start().strip_prefix('"')?.split('"').next()?;
        ReleaseVersion::from_str(version).ok()
    })
}

/// The `cargo` process for `argv`, found on `PATH`, not yet started.
fn cargo(argv: &[String]) -> Command {
    let mut command = Command::new(argv.first().map_or("cargo", String::as_str));
    let _command = command.args(argv.iter().skip(1));
    command
}

/// Runs the install `argv` from `cargo_home`, standard input closed and standard output left
/// alone. Cargo's standard error is shown as it arrives and kept to recognize one failure.
///
/// Effect: runs cargo, which reads and writes the network and `$CARGO_HOME`.
///
/// # Errors
///
/// Returns `cli-upgrade-no-cargo` when cargo cannot start and `cli-upgrade-cargo-failed` when it
/// exits with a failure.
fn install(argv: &[String], cargo_home: &Path, json: bool) -> Result<(), CliError> {
    let mut child = cargo(argv)
        .current_dir(cargo_home)
        .stdin(Stdio::null())
        .stdout(if json {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| no_cargo(&error))?;
    let mut shown = Vec::new();
    std::thread::scope(|scope| {
        // Under JSON, standard output holds the one report, so cargo's goes to standard error.
        let _forwarder = child.stdout.take().map(|mut stdout| {
            scope.spawn(move || {
                let mut chunk = [0_u8; 4096];
                // Keep reading after a write error, so cargo is never left with a closed pipe.
                while let Ok(read @ 1..) = stdout.read(&mut chunk) {
                    let mut out = io::stderr();
                    let _written = out
                        .write_all(chunk.get(..read).unwrap_or_default())
                        .and_then(|()| out.flush());
                }
            })
        });
        if let Some(mut stderr) = child.stderr.take() {
            let mut chunk = [0_u8; 4096];
            loop {
                let read = match stderr.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                let bytes = chunk.get(..read).unwrap_or_default();
                shown.extend_from_slice(bytes);
                let mut out = io::stderr();
                // Cargo's progress is best effort: a closed stderr must not stop the install.
                let _written = out.write_all(bytes).and_then(|()| out.flush());
            }
        }
    });
    let status = child.wait().map_err(|error| no_cargo(&error))?;
    if status.success() {
        Ok(())
    } else {
        Err(cargo_failed(argv, status, &String::from_utf8_lossy(&shown)))
    }
}

/// The error for cargo that cannot start.
fn no_cargo(error: &io::Error) -> CliError {
    CliError::new(
        "cli-upgrade-no-cargo",
        format!("cannot run `cargo`: {}", cause(error)),
        "Put `cargo` on PATH (for example `export PATH=$HOME/.cargo/bin:$PATH`), then run `supdoc upgrade` again",
        ExitStatus::Io,
    )
}

/// The error for a cargo that failed. When `stderr` has cargo's not-found shape, the help adds
/// that the version is not on crates.io; cargo's own message is never rewritten.
fn cargo_failed(argv: &[String], status: std::process::ExitStatus, stderr: &str) -> CliError {
    let help = if stderr.contains(NOT_FOUND_SHAPE) {
        "That version does not exist on crates.io; run `supdoc upgrade` with no version for the latest"
    } else {
        "Read cargo's message above, fix what it names, then run `supdoc upgrade` again"
    };
    CliError::new(
        "cli-upgrade-cargo-failed",
        format!("`{}` failed: {status}", shell_line(argv)),
        help,
        ExitStatus::Failed,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_404_for_sha256sums_is_no_asset_and_every_other_failure_is_a_fetch_failure() {
        let url = "https://github.com/aidenzepp/supdoc/releases/download/v0.0.2/SHA256SUMS";
        let status = |status| fetch::FetchError::Status {
            url: url.to_owned(),
            status,
        };
        let transport = fetch::FetchError::Transport {
            url: url.to_owned(),
            message: "could not resolve host".to_owned(),
        };
        let code = |error| sums_fetch_error(url, None, error).code();
        assert_eq!(code(status(404)), "cli-upgrade-no-asset");
        assert_eq!(code(status(403)), "cli-upgrade-fetch-failed");
        assert_eq!(code(status(500)), "cli-upgrade-fetch-failed");
        assert_eq!(code(transport), "cli-upgrade-fetch-failed");
    }

    fn version(text: &str) -> ReleaseVersion {
        ReleaseVersion::from_str(text).expect("the test version should parse")
    }

    #[test]
    fn versions_accept_the_semver_shape_and_refuse_everything_else() {
        for good in [
            "0.3.1",
            "10.20.30",
            "1.0.0-rc.1",
            "1.0.0+build.5",
            "1.0.0-a-b.2+x",
        ] {
            assert!(
                ReleaseVersion::from_str(good).is_ok(),
                "`{good}` should parse"
            );
        }
        for bad in [
            "", "1", "1.2", "1.2.3.4", "01.2.3", "1.2.3-", "1.2.3-01", "1.2.3+", "a.b.c", "1.2.3 ",
            "=1.2.3",
        ] {
            assert!(
                ReleaseVersion::from_str(bad).is_err(),
                "`{bad}` should be refused"
            );
        }
    }

    #[test]
    fn a_leading_v_is_refused_with_the_version_to_write() {
        let error = ReleaseVersion::from_str("v0.3.1").expect_err("`v0.3.1` should be refused");
        assert_eq!(error, "write `0.3.1`, not `v0.3.1`");
    }

    #[test]
    fn directions_follow_semver_precedence() {
        let current = version("0.3.1");
        assert_eq!(current.direction_to(&version("0.4.0")), Direction::Upgrade);
        assert_eq!(
            current.direction_to(&version("0.3.0")),
            Direction::Downgrade
        );
        assert_eq!(
            current.direction_to(&version("0.3.1+build")),
            Direction::Same
        );
        assert_eq!(
            current.direction_to(&version("0.3.1-rc.1")),
            Direction::Downgrade
        );
        assert_eq!(
            version("0.3.1-rc.2").direction_to(&version("0.3.1-rc.10")),
            Direction::Upgrade
        );
    }

    #[test]
    fn the_search_line_gives_the_version() {
        let output = "supdoc = \"0.3.2\"    # Track links\n... and 3 crates more\n";
        assert_eq!(parse_search(output), Some(version("0.3.2")));
        assert_eq!(parse_search("supdoc-extra = \"1.0.0\"\n"), None);
        assert_eq!(parse_search("error: nothing\n"), None);
    }

    #[test]
    fn the_install_argv_pins_the_exact_version_locked_and_forced() {
        assert_eq!(
            shell_line(&install_argv(
                &version("0.3.2+b"),
                Path::new("/home/u/.cargo")
            )),
            "cargo install supdoc --locked --version =0.3.2 --force --root /home/u/.cargo --color never"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_cargo_home_decision_names_why_none_is_usable() {
        use std::os::unix::ffi::OsStringExt as _;
        let os = OsString::from;
        assert_eq!(
            cargo_home_from(Some(os("/c")), None),
            Ok(PathBuf::from("/c"))
        );
        assert_eq!(
            cargo_home_from(None, Some(os("/h"))),
            Ok(PathBuf::from("/h/.cargo"))
        );
        assert_eq!(
            cargo_home_from(Some(os("rel")), Some(os("/h"))),
            Err(CargoHomeProblem::RelativeSet)
        );
        assert_eq!(
            cargo_home_from(Some(os("")), Some(os("rel"))),
            Err(CargoHomeProblem::Unset)
        );
        assert_eq!(cargo_home_from(None, None), Err(CargoHomeProblem::Unset));
        let latin1 = OsString::from_vec(b"/home/caf\xe9".to_vec());
        assert_eq!(
            cargo_home_from(Some(latin1.clone()), None),
            Err(CargoHomeProblem::NotUtf8)
        );
        assert_eq!(
            cargo_home_from(None, Some(latin1)),
            Err(CargoHomeProblem::NotUtf8)
        );
        assert!(
            no_cargo_home(CargoHomeProblem::NotUtf8)
                .message()
                .contains("CARGO_HOME")
        );
    }

    #[test]
    fn a_source_with_spaces_is_named_whole_and_quoted() {
        let record = r#"{"installs":{"supdoc 0.3.1 (path+file:///Users/x/my dir/supdoc)":{"bins":["supdoc"]}}}"#;
        let error = recorded_version(record, "/x").expect_err("a path install should be refused");
        assert!(
            error
                .message()
                .contains("'path+file:///Users/x/my dir/supdoc'"),
            "{error}"
        );
    }

    #[test]
    fn a_record_from_git_is_refused_naming_its_source() {
        let record = r#"{"installs":{"supdoc 0.3.1 (git+https://example.com/supdoc#abc)":{"bins":["supdoc"]}}}"#;
        let error = recorded_version(record, "/x").expect_err("a git install should be refused");
        assert!(
            error
                .message()
                .contains("git+https://example.com/supdoc#abc"),
            "{error}"
        );
    }
}
