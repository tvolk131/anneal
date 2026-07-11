//! Native fixed-output downloads (§FOD).
//!
//! The executor fetches pinned blobs in-process over a pure-Rust stack (ureq →
//! rustls/ring) with Mozilla's root store compiled in (webpki-roots). No curl,
//! no sandbox, no host trust configuration — no `SSL_CERT_FILE`, no
//! `/etc/ssl`, no OS keychain — so a fetch behaves identically on every
//! machine, including ones with no Nix store at all. This is the Buck2/Bazel
//! shape: downloading a pinned artifact is kernel infrastructure, not an inner
//! tool to wrap.
//!
//! Trust: the sha256 pin carries the entire integrity guarantee
//! (`run_fixed_output` verifies before the result is used, failing closed), so
//! TLS contributes availability and privacy only. A root store as fresh as the
//! anneal release is therefore sufficient — a stale root can produce a clear
//! availability failure, never wrong bytes.
//!
//! The executor process keeps the user's environment (unlike the scrubbed
//! sandbox), so standard proxy variables work here — a fix, incidentally, for
//! proxied environments where the sandboxed-curl fetch could never connect.

use std::fmt;
use std::time::Duration;

/// Transport attempts per URL (curl's `--retry 3` equivalent). Definitive
/// HTTP failures (4xx) do not retry; transport errors and 5xx/429 back off
/// linearly.
const ATTEMPTS: u32 = 3;

/// Per-attempt backoff base. Attempt `n` (1-based) sleeps `n × BACKOFF` after
/// a retryable failure.
const BACKOFF: Duration = Duration::from_millis(250);

/// Hard ceiling on a fetched blob (a `.crate` is typically well under 50 MiB;
/// this guards against a pathological response, not a real artifact).
const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Why a download definitively failed (after retries, where applicable).
#[derive(Debug)]
pub struct FetchError {
    url: String,
    attempts: u32,
    last: String,
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "fetching {} failed after {} attempt(s): {}",
            self.url, self.attempts, self.last
        )
    }
}

impl std::error::Error for FetchError {}

/// Download `url` fully into memory. Follows redirects (ureq's default), and
/// retries transport-level failures up to [`ATTEMPTS`] times. Verification
/// against the pin is the caller's job — this function only moves bytes.
pub(crate) fn download(url: &str) -> Result<Vec<u8>, FetchError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(600)))
        .build()
        .into();

    let mut last = String::new();
    let mut attempts = 0;
    for attempt in 1..=ATTEMPTS {
        attempts = attempt;
        match try_download(&agent, url) {
            Ok(bytes) => return Ok(bytes),
            Err(Retry::No(error)) => {
                return Err(FetchError {
                    url: url.to_owned(),
                    attempts: attempt,
                    last: error,
                });
            }
            Err(Retry::Yes(error)) => {
                last = error;
                if attempt < ATTEMPTS {
                    std::thread::sleep(BACKOFF * attempt);
                }
            }
        }
    }
    Err(FetchError {
        url: url.to_owned(),
        attempts,
        last,
    })
}

/// A single attempt's failure, classified for the retry loop.
enum Retry {
    /// Transient (transport error, 5xx, 429): try again.
    Yes(String),
    /// Definitive (4xx): the artifact is not there; retrying cannot help.
    No(String),
}

fn try_download(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, Retry> {
    let mut response = agent.get(url).call().map_err(|error| match &error {
        ureq::Error::StatusCode(code) if *code == 429 || *code >= 500 => {
            Retry::Yes(error.to_string())
        }
        ureq::Error::StatusCode(_) => Retry::No(error.to_string()),
        _ => Retry::Yes(error.to_string()),
    })?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_BYTES)
        .read_to_vec()
        .map_err(|error| Retry::Yes(format!("reading response body: {error}")))
}
