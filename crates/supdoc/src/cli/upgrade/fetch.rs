//! The one HTTPS fetch behind `supdoc upgrade` for a downloaded binary.
//!
//! Every byte comes through [`fetch`]. The policy lives in two pure functions, [`check_initial`]
//! and [`check_redirect`], so the hosts, the scheme and the redirect count are unit-tested without
//! a network: HTTPS only, the first request goes to `github.com`, at most [`MAX_REDIRECTS`]
//! redirects, and each redirect target is `https://` on one of [`REDIRECT_HOSTS`]. The client
//! never reads a proxy or a base URL from the environment. The bytes come from the system `curl`,
//! run by absolute path with a cleared environment, so the operating system's TLS and trust store
//! do the handshake. Debug builds alone honour `SUPDOC_TEST_RELEASE_DIR`, which serves files from
//! a directory instead of the network.

use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The largest `SHA256SUMS` body, in bytes.
pub(crate) const MAX_SUMS_BYTES: u64 = 64 * 1024;

/// The largest release archive body, in bytes.
pub(crate) const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

/// How many redirects one fetch follows before it gives up.
const MAX_REDIRECTS: usize = 5;

/// The only host the first request may go to.
const INITIAL_HOST: &str = "github.com";

/// The hosts a redirect may name.
const REDIRECT_HOSTS: [&str; 3] = [
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];

/// Where Unix systems keep `curl`, in the order tried.
#[cfg(unix)]
const CURL_CANDIDATES: [&str; 4] = [
    "/usr/bin/curl",
    "/bin/curl",
    "/usr/local/bin/curl",
    "/opt/homebrew/bin/curl",
];

/// How much of the end of curl's stderr is kept, in bytes. The status line is last and an error
/// message is short; everything earlier is read and dropped so a chatty curl never blocks on a
/// full pipe.
const STDERR_KEEP: usize = 64 * 1024;

/// The oldest curl whose `--write-out` knows `%{stderr}`.
const MIN_CURL: &str = "7.63.0";

/// The debug-only variable that serves release files from a directory.
#[cfg(debug_assertions)]
const TEST_DIR_VAR: &str = "SUPDOC_TEST_RELEASE_DIR";

/// Why a fetch was refused or failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FetchError {
    /// The URL is not `https://host/...` with a bare host.
    #[error("`{0}` is not an https:// URL with a plain host")]
    NotHttps(String),
    /// The URL names a host the release policy does not allow.
    #[error("`{url}` names host `{host}`, which is not an allowed release host")]
    HostNotAllowed {
        /// The refused URL.
        url: String,
        /// Its host.
        host: String,
    },
    /// The server redirected more than [`MAX_REDIRECTS`] times.
    #[error("the download redirected more than {MAX_REDIRECTS} times")]
    TooManyRedirects,
    /// A redirect had no usable `Location`.
    #[error("the server redirected without a `Location` header")]
    NoLocation,
    /// The server answered with a status other than 200 or a redirect.
    #[error("the server answered `{url}` with status {status}")]
    Status {
        /// The URL asked for.
        url: String,
        /// The HTTP status code.
        status: u16,
    },
    /// The body is larger than the cap.
    #[error("the response for `{url}` is over the {limit} byte cap")]
    TooLarge {
        /// The URL asked for.
        url: String,
        /// The cap.
        limit: u64,
    },
    /// No `curl` exists at any fixed path.
    #[error("supdoc upgrade needs curl; download the release asset by hand: {url}")]
    NoCurl {
        /// The URL asked for.
        url: String,
    },
    /// The transport failed.
    #[error("could not fetch `{url}`: {message}")]
    Transport {
        /// The URL asked for.
        url: String,
        /// The transport's message.
        message: String,
    },
    /// The debug-only test directory could not serve the file.
    #[cfg(debug_assertions)]
    #[error("could not read test release file for `{url}`: {message}")]
    TestFile {
        /// The URL asked for.
        url: String,
        /// The I/O message.
        message: String,
    },
}

/// The host of an `https://` URL, or `None` for any other shape.
///
/// Userinfo, a port, a backslash, whitespace, control characters and an empty host all give
/// `None`, so the host that is compared is the host a client connects to.
fn https_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    if rest
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest.get(..end)?;
    if host.is_empty() || host.contains(['@', ':', '[', ']']) {
        return None;
    }
    Some(host)
}

/// Whether `host` is one of `allowed`, ignoring ASCII case.
fn host_in(host: &str, allowed: &[&str]) -> bool {
    allowed.iter().any(|name| name.eq_ignore_ascii_case(host))
}

/// Admits the first request: `https://` on `github.com`.
///
/// # Errors
///
/// Returns [`FetchError::NotHttps`] or [`FetchError::HostNotAllowed`].
fn check_initial(url: &str) -> Result<(), FetchError> {
    let host = https_host(url).ok_or_else(|| FetchError::NotHttps(url.to_owned()))?;
    if host_in(host, &[INITIAL_HOST]) {
        return Ok(());
    }
    Err(FetchError::HostNotAllowed {
        url: url.to_owned(),
        host: host.to_owned(),
    })
}

/// Admits the `redirect_number`th redirect (counting from 1) to `location`.
///
/// # Errors
///
/// Returns [`FetchError::TooManyRedirects`] past the fifth, [`FetchError::NotHttps`] for a
/// target that is not `https://` with a plain host, and [`FetchError::HostNotAllowed`] for a host
/// off the list.
fn check_redirect(location: &str, redirect_number: usize) -> Result<(), FetchError> {
    if redirect_number > MAX_REDIRECTS {
        return Err(FetchError::TooManyRedirects);
    }
    let host = https_host(location).ok_or_else(|| FetchError::NotHttps(location.to_owned()))?;
    if host_in(host, &REDIRECT_HOSTS) {
        return Ok(());
    }
    Err(FetchError::HostNotAllowed {
        url: location.to_owned(),
        host: host.to_owned(),
    })
}

/// The first of `candidates` that `exists` accepts. No `PATH` lookup ever happens.
fn choose_curl(candidates: &[PathBuf], exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    candidates.iter().find(|path| exists(path)).cloned()
}

/// The fixed paths `curl` may live at on this operating system.
fn curl_candidates() -> Vec<PathBuf> {
    #[cfg(unix)]
    {
        CURL_CANDIDATES.iter().map(PathBuf::from).collect()
    }
    #[cfg(windows)]
    {
        vec![
            crate::process::recorded_system_root()
                .join("System32")
                .join("curl.exe"),
        ]
    }
}

/// The arguments for one request: `-q` first so no `.curlrc` applies, `--globoff` so `[` and `{`
/// in a URL are not expanded, https only, TLS 1.2 or
/// newer, no redirect followed by curl itself, the size cap, timeouts, and a status line on
/// stderr after the body.
fn curl_args(url: &str, cap: u64) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-q",
        "--globoff",
        "--proto",
        "=https",
        "--tlsv1.2",
        "--fail",
        "--silent",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.extend(
        [
            "--show-error",
            "--max-redirs",
            "0",
            "--max-filesize",
            &cap.to_string(),
            "--connect-timeout",
            "30",
            "--max-time",
            "600",
            "--write-out",
            "%{stderr}\n%{http_code} %{redirect_url}\n",
            "--",
            url,
        ]
        .iter()
        .map(OsString::from),
    );
    args
}

/// The status code and redirect URL from the last line curl wrote to stderr.
fn parse_status(stderr: &str) -> Option<(u16, String)> {
    let line = stderr.lines().rev().find(|line| !line.trim().is_empty())?;
    let (code, location) = line.split_once(' ').unwrap_or((line, ""));
    Some((code.trim().parse().ok()?, location.trim().to_owned()))
}

/// What one curl request returned.
struct Hop {
    /// The HTTP status code.
    status: u16,
    /// The redirect URL, empty when there is none.
    location: String,
    /// The body, at most the cap.
    body: Vec<u8>,
}

/// Kills and reaps `child` after a failure so nothing is left running.
fn abort(child: &mut std::process::Child) {
    let _killed = child.kill();
    let _reaped = child.wait();
}

/// Runs curl once for `url` and reads at most `cap` body bytes.
///
/// Stdout and stderr are drained at the same time, so a curl that writes a lot to stderr cannot
/// stall on a full pipe.
///
/// # Errors
///
/// Returns [`FetchError::TooLarge`] when the body passes `cap`, and [`FetchError::Transport`]
/// when curl cannot start, a read fails, curl exits with an error, or it is too old to print the
/// status line.
fn run_curl(curl: &Path, url: &str, cap: u64) -> Result<Hop, FetchError> {
    let transport = |message: String| FetchError::Transport {
        url: url.to_owned(),
        message,
    };
    let mut command = Command::new(curl);
    let _args = command
        .args(curl_args(url, cap))
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    let _root = command.env("SystemRoot", crate::process::recorded_system_root());
    let mut child = command
        .spawn()
        .map_err(|error| transport(error.to_string()))?;
    let stderr_reader = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut kept: Vec<u8> = Vec::new();
            let mut chunk = [0_u8; 8192];
            while let Ok(read) = pipe.read(&mut chunk) {
                let Some(bytes) = chunk.get(..read).filter(|bytes| !bytes.is_empty()) else {
                    break;
                };
                kept.extend_from_slice(bytes);
                let excess = kept.len().saturating_sub(STDERR_KEEP);
                drop(kept.drain(..excess));
            }
            String::from_utf8_lossy(&kept).into_owned()
        })
    });
    let mut body = Vec::new();
    if let Some(stdout) = child.stdout.take()
        && let Err(error) = stdout.take(cap.saturating_add(1)).read_to_end(&mut body)
    {
        abort(&mut child);
        return Err(transport(error.to_string()));
    }
    if u64::try_from(body.len()).map_or(true, |len| len > cap) {
        abort(&mut child);
        return Err(FetchError::TooLarge {
            url: url.to_owned(),
            limit: cap,
        });
    }
    let stderr = stderr_reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    let exit = child.wait().map_err(|error| transport(error.to_string()))?;
    if !exit.success() {
        let detail = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("curl failed");
        return Err(transport(format!("{detail} ({exit})")));
    }
    let (status, location) = parse_status(&stderr).ok_or_else(|| {
        transport(format!(
            "curl printed no status line, so it is too old: supdoc needs curl {MIN_CURL} or newer"
        ))
    })?;
    Ok(Hop {
        status,
        location,
        body,
    })
}

/// The bytes at an `https://github.com/...` URL, following allowed redirects, at most `cap` long.
///
/// # Errors
///
/// Returns a [`FetchError`] when the URL or any redirect breaks the host policy, when the server
/// answers with anything but 200 or a redirect, when the body passes `cap`, when no `curl`
/// exists, or on a transport failure.
pub(crate) fn fetch(url: &str, cap: u64) -> Result<Vec<u8>, FetchError> {
    #[cfg(debug_assertions)]
    if let Some(dir) = std::env::var_os(TEST_DIR_VAR) {
        return fetch_from_dir(Path::new(&dir), url, cap);
    }
    check_initial(url)?;
    let curl = choose_curl(&curl_candidates(), Path::exists).ok_or_else(|| FetchError::NoCurl {
        url: url.to_owned(),
    })?;
    fetch_with(&curl, url, cap)
}

/// The fetch loop over the curl at `curl`: one request per hop, each redirect checked before the
/// next request runs.
///
/// # Errors
///
/// Returns a [`FetchError`] as [`fetch`] does, except for a missing curl.
fn fetch_with(curl: &Path, url: &str, cap: u64) -> Result<Vec<u8>, FetchError> {
    check_initial(url)?;
    let mut current = url.to_owned();
    let mut redirects = 0;
    loop {
        let hop = run_curl(curl, &current, cap)?;
        if matches!(hop.status, 301 | 302 | 303 | 307 | 308) {
            redirects += 1;
            if hop.location.is_empty() {
                return Err(FetchError::NoLocation);
            }
            check_redirect(&hop.location, redirects)?;
            current = hop.location;
            continue;
        }
        if hop.status != 200 {
            return Err(FetchError::Status {
                url: current,
                status: hop.status,
            });
        }
        return Ok(hop.body);
    }
}

/// Serves the last path segment of `url` from `dir`, for debug builds under test.
///
/// # Errors
///
/// Returns [`FetchError::TestFile`] for a missing or unreadable file and
/// [`FetchError::TooLarge`] for one over `cap`.
#[cfg(debug_assertions)]
fn fetch_from_dir(dir: &Path, url: &str, cap: u64) -> Result<Vec<u8>, FetchError> {
    let test_file = |message: String| FetchError::TestFile {
        url: url.to_owned(),
        message,
    };
    let name = url
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| test_file("the URL has no file name".to_owned()))?;
    let file = std::fs::File::open(dir.join(name)).map_err(|error| test_file(error.to_string()))?;
    let mut bytes = Vec::new();
    let _read = file
        .take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| test_file(error.to_string()))?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > cap) {
        return Err(FetchError::TooLarge {
            url: url.to_owned(),
            limit: cap,
        });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_is_refused_at_the_start_and_on_a_redirect() {
        assert!(matches!(
            check_initial("http://github.com/aidenzepp/supdoc"),
            Err(FetchError::NotHttps(_))
        ));
        assert!(matches!(
            check_redirect("http://github.com/x", 1),
            Err(FetchError::NotHttps(_))
        ));
    }

    #[test]
    fn a_redirect_to_a_host_off_the_list_is_refused() {
        for target in [
            "https://evil.example/x",
            "https://github.com.evil.example/x",
            "https://evilgithub.com/x",
            "https://githubusercontent.com/x",
            "https://user@github.com/x",
            "https://github.com:8443/x",
            "https://github.com\\@evil.example/x",
            "/relative/path",
        ] {
            assert!(
                check_redirect(target, 1).is_err(),
                "{target} must be refused"
            );
        }
    }

    #[test]
    fn the_first_request_must_be_github_com() {
        assert!(check_initial("https://github.com/aidenzepp/supdoc/releases").is_ok());
        assert!(matches!(
            check_initial("https://objects.githubusercontent.com/x"),
            Err(FetchError::HostNotAllowed { .. })
        ));
    }

    #[test]
    fn the_sixth_redirect_is_refused_and_the_fifth_is_not() {
        let target = "https://github.com/x";
        assert!(check_redirect(target, 5).is_ok());
        assert!(matches!(
            check_redirect(target, 6),
            Err(FetchError::TooManyRedirects)
        ));
    }

    #[test]
    fn the_caps_are_64_kib_for_sums_and_64_mib_for_the_archive() {
        assert_eq!(MAX_SUMS_BYTES, 65_536);
        assert_eq!(MAX_ARCHIVE_BYTES, 67_108_864);
    }

    #[test]
    fn the_three_listed_hosts_are_accepted() {
        for target in [
            "https://github.com/a/b",
            "https://release-assets.githubusercontent.com/a?token=1",
            "https://objects.githubusercontent.com/a#frag",
            "https://GitHub.com/a",
        ] {
            assert!(
                check_redirect(target, 1).is_ok(),
                "{target} must be accepted"
            );
        }
    }

    #[test]
    fn curl_runs_with_q_first_https_only_no_redirects_and_the_cap() {
        let args: Vec<String> = curl_args("https://github.com/x", 4096)
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.first().map(String::as_str), Some("-q"));
        assert_eq!(args.get(1).map(String::as_str), Some("--globoff"));
        let pair = |flag: &str, value: &str| {
            args.windows(2).any(|two| {
                two.first().map(String::as_str) == Some(flag)
                    && two.get(1).map(String::as_str) == Some(value)
            })
        };
        assert!(pair("--proto", "=https"), "{args:?}");
        assert!(pair("--max-redirs", "0"), "{args:?}");
        assert!(pair("--max-filesize", "4096"), "{args:?}");
        assert!(args.iter().any(|arg| arg == "--tlsv1.2"), "{args:?}");
        assert_eq!(
            args.last().map(String::as_str),
            Some("https://github.com/x")
        );
    }

    #[test]
    fn the_first_existing_curl_candidate_wins_and_none_is_none() {
        let list = [
            PathBuf::from("/a/curl"),
            PathBuf::from("/b/curl"),
            PathBuf::from("/c/curl"),
        ];
        let only = |wanted: &'static [&'static str]| {
            move |path: &Path| wanted.iter().any(|w| path == Path::new(w))
        };
        assert_eq!(
            choose_curl(&list, only(&["/b/curl", "/c/curl"])),
            Some(PathBuf::from("/b/curl"))
        );
        assert_eq!(choose_curl(&list, only(&[])), None);
    }

    #[test]
    fn the_status_line_gives_the_code_and_the_redirect_url() {
        assert_eq!(
            parse_status("noise\n302 https://objects.githubusercontent.com/a\n"),
            Some((302, "https://objects.githubusercontent.com/a".to_owned()))
        );
        assert_eq!(parse_status("\n200 \n"), Some((200, String::new())));
        assert_eq!(parse_status("curl: oops"), None);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn the_test_directory_serves_a_file_and_enforces_the_cap() {
        let dir = tempfile::tempdir().expect("the test directory should be created");
        std::fs::write(dir.path().join("SHA256SUMS"), b"12345").expect("the fixture should write");
        let url = "https://github.com/aidenzepp/supdoc/releases/latest/download/SHA256SUMS";
        assert_eq!(
            fetch_from_dir(dir.path(), url, 5).expect("a body at the cap is served"),
            b"12345"
        );
        assert!(matches!(
            fetch_from_dir(dir.path(), url, 4),
            Err(FetchError::TooLarge { .. })
        ));
        assert!(matches!(
            fetch_from_dir(dir.path(), "https://github.com/a/..", 4),
            Err(FetchError::TestFile { .. })
        ));
    }

    /// A stub curl for the fetch loop: writes each URL it is asked for to a log, then answers from
    /// `rules` as (url, status, location, body); `extra` is more `case` arms.
    #[cfg(unix)]
    fn stub_curl(dir: &Path, rules: &[(&str, u16, &str, &str)], extra: &str) -> (PathBuf, PathBuf) {
        use std::fmt::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let log = dir.join("requests.log");
        let mut script = format!(
            "#!/bin/sh\nfor a; do u=$a; done\nprintf '%s\\n' \"$u\" >> '{}'\ncase \"$u\" in\n",
            log.display()
        );
        for (url, status, location, body) in rules {
            let _arm = writeln!(
                script,
                "'{url}') printf '%s' '{body}'; printf '\\n%s %s\\n' '{status}' '{location}' >&2 ;;"
            );
        }
        script.push_str(extra);
        script.push_str("*) printf '\\n404 \\n' >&2; exit 22 ;;\nesac\n");
        let path = dir.join("curl");
        std::fs::write(&path, script).expect("the stub curl should be written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("the stub curl should become executable");
        (path, log)
    }

    #[cfg(unix)]
    fn requests(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn a_redirect_off_the_list_is_refused_before_its_request_runs() {
        let dir = tempfile::tempdir().expect("the stub directory should be created");
        let (curl, log) = stub_curl(
            dir.path(),
            &[
                ("https://github.com/a", 302, "https://evil.example/b", ""),
                ("https://evil.example/b", 200, "", "pwned"),
            ],
            "",
        );
        let result = fetch_with(&curl, "https://github.com/a", 100);
        assert!(
            matches!(result, Err(FetchError::HostNotAllowed { .. })),
            "{result:?}"
        );
        assert_eq!(requests(&log), ["https://github.com/a"]);
    }

    #[cfg(unix)]
    #[test]
    fn five_redirects_then_a_body_succeed_and_a_sixth_redirect_is_refused() {
        let dir = tempfile::tempdir().expect("the stub directory should be created");
        let chain = |last: (u16, &'static str, &'static str)| {
            let urls: Vec<String> = (0..=6)
                .map(|n| format!("https://github.com/r{n}"))
                .collect();
            let rules: Vec<(String, u16, String, String)> = urls
                .iter()
                .enumerate()
                .map(|(n, url)| match (n, urls.get(n + 1)) {
                    (5, _) => (url.clone(), last.0, last.2.to_owned(), last.1.to_owned()),
                    (_, Some(next)) => (url.clone(), 302, next.clone(), String::new()),
                    (_, None) => (url.clone(), 200, String::new(), "end".to_owned()),
                })
                .collect();
            rules
        };
        let run = |rules: Vec<(String, u16, String, String)>| {
            let borrowed: Vec<(&str, u16, &str, &str)> = rules
                .iter()
                .map(|(u, c, l, b)| (u.as_str(), *c, l.as_str(), b.as_str()))
                .collect();
            let (curl, log) = stub_curl(dir.path(), &borrowed, "");
            let _gone = std::fs::remove_file(&log);
            let result = fetch_with(&curl, "https://github.com/r0", 100);
            (result, requests(&log))
        };
        let (ok, seen) = run(chain((200, "five", "")));
        assert_eq!(ok.expect("five redirects are allowed"), b"five");
        assert_eq!(seen.len(), 6);
        let (refused, seen) = run(chain((302, "", "https://github.com/r6")));
        assert!(
            matches!(refused, Err(FetchError::TooManyRedirects)),
            "{refused:?}"
        );
        assert_eq!(seen.len(), 6, "the seventh request must not run: {seen:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_body_past_the_cap_is_refused() {
        let dir = tempfile::tempdir().expect("the stub directory should be created");
        let (curl, _log) = stub_curl(
            dir.path(),
            &[("https://github.com/big", 200, "", "123456789")],
            "",
        );
        assert!(matches!(
            fetch_with(&curl, "https://github.com/big", 4),
            Err(FetchError::TooLarge { .. })
        ));
        assert_eq!(
            fetch_with(&curl, "https://github.com/big", 9).expect("a body at the cap is served"),
            b"123456789"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_curl_that_floods_stderr_does_not_stall_the_fetch() {
        let dir = tempfile::tempdir().expect("the stub directory should be created");
        let flood = format!(
            "'https://github.com/flood') i=0; while [ $i -lt 3000 ]; do printf '%s\\n' '{}' >&2; i=$((i+1)); done; printf 'ok'; printf '\\n200 \\n' >&2 ;;\n",
            "x".repeat(100)
        );
        let (curl, _log) = stub_curl(dir.path(), &[], &flood);
        assert_eq!(
            fetch_with(&curl, "https://github.com/flood", 100)
                .expect("a flood on stderr is drained"),
            b"ok"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_curl_with_no_status_line_is_called_too_old() {
        let dir = tempfile::tempdir().expect("the stub directory should be created");
        let (curl, _log) = stub_curl(
            dir.path(),
            &[],
            "'https://github.com/old') printf 'body' ;;\n",
        );
        let error = fetch_with(&curl, "https://github.com/old", 100)
            .expect_err("a curl without a status line is refused");
        let text = error.to_string();
        assert!(text.contains("too old") && text.contains("7.63"), "{text}");
    }
}
