//! The one HTTPS fetch behind `supdoc upgrade` for a downloaded binary.
//!
//! Every byte comes through [`fetch`]. The policy lives in two pure functions, [`check_initial`]
//! and [`check_redirect`], so the hosts, the scheme and the redirect count are unit-tested without
//! a network: HTTPS only, the first request goes to `github.com`, at most [`MAX_REDIRECTS`]
//! redirects, and each redirect target is `https://` on one of [`REDIRECT_HOSTS`]. The client
//! never reads a proxy or a base URL from the environment. Debug builds alone honour
//! `SUPDOC_TEST_RELEASE_DIR`, which serves files from a directory instead of the network.

use std::sync::Arc;

use ureq::config::Config;
use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};

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
    /// The system offers no root certificate to verify the server with.
    #[error("the system has no root certificates to verify the download server with")]
    NoRoots,
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

/// The bytes at an `https://github.com/...` URL, following allowed redirects, at most `cap` long.
///
/// # Errors
///
/// Returns a [`FetchError`] when the URL or any redirect breaks the host policy, when the server
/// answers with anything but 200 or a redirect, when the body passes `cap`, or on a transport
/// failure.
pub(crate) fn fetch(url: &str, cap: u64) -> Result<Vec<u8>, FetchError> {
    #[cfg(debug_assertions)]
    if let Some(dir) = std::env::var_os(TEST_DIR_VAR) {
        return fetch_from_dir(std::path::Path::new(&dir), url, cap);
    }
    check_initial(url)?;
    let agent = agent()?;
    let mut current = url.to_owned();
    let mut redirects = 0;
    loop {
        let mut response = agent
            .get(&current)
            .call()
            .map_err(|error| transport(&current, &error))?;
        let status = response.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            redirects += 1;
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or(FetchError::NoLocation)?
                .to_owned();
            check_redirect(&location, redirects)?;
            current = location;
            continue;
        }
        if status != 200 {
            return Err(FetchError::Status {
                url: current,
                status,
            });
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(cap)
            .read_to_vec()
            .map_err(|error| match error {
                ureq::Error::BodyExceedsLimit(_) => FetchError::TooLarge {
                    url: current.clone(),
                    limit: cap,
                },
                other => transport(&current, &other),
            })?;
        return Ok(body);
    }
}

/// A client that checks the host policy itself: no redirects of its own, no proxy, https only,
/// the operating system's root certificates, and ring as the crypto provider.
///
/// # Errors
///
/// Returns [`FetchError::NoRoots`] when the system has no usable root certificate.
fn agent() -> Result<ureq::Agent, FetchError> {
    let roots: Vec<Certificate<'static>> = rustls_native_certs::load_native_certs()
        .certs
        .into_iter()
        .map(|der| Certificate::from_der(der.as_ref()).to_owned())
        .collect();
    if roots.is_empty() {
        return Err(FetchError::NoRoots);
    }
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .root_certs(RootCerts::new_with_certs(&roots))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    let config = Config::builder()
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .tls_config(tls)
        .build();
    Ok(ureq::Agent::new_with_config(config))
}

/// Wraps a ureq error with the URL it came from.
fn transport(url: &str, error: &ureq::Error) -> FetchError {
    FetchError::Transport {
        url: url.to_owned(),
        message: error.to_string(),
    }
}

/// Serves the last path segment of `url` from `dir`, for debug builds under test.
///
/// # Errors
///
/// Returns [`FetchError::TestFile`] for a missing or unreadable file and
/// [`FetchError::TooLarge`] for one over `cap`.
#[cfg(debug_assertions)]
fn fetch_from_dir(dir: &std::path::Path, url: &str, cap: u64) -> Result<Vec<u8>, FetchError> {
    use std::io::Read as _;

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
}
