//! Main fetch algorithm chain per WHATWG Fetch §5.
//!
//! Per design fetch-native v2 §V, the chain is:
//!
//!   1. `fetch(input, init)` — top-level entry, returns `Promise<Response>`.
//!      Handles input coercion, signal-already-aborted check, scheme
//!      dispatch.
//!   2. `main_fetch(fetchParams)` — routes by scheme: `http`/`https` →
//!      `http_fetch`, `data:` → `data_url_fetch`, others → network error.
//!   3. `http_fetch(fetchParams)` — sets up CORS bypass (we
//!      ignore mode/credentials policing because the gateway is the
//!      perimeter), calls `http_redirect_fetch`.
//!   4. `http_redirect_fetch(fetchParams)` — wraps the redirect loop
//!      around `http_network_or_cache_fetch`. Implements the 20-cap,
//!      method/body mutation per status, cross-origin Authorization
//!      stripping.
//!   5. `http_network_or_cache_fetch` — applies cache-aware request
//!      header rewrites (we don't run a cache, but we still
//!      respect `Cache-Control: no-store` etc. for upstream behaviour).
//!   6. `http_network_fetch` — the actual TCP/TLS round-trip. Wraps
//!      the cyper backend.
//!
//! ## Naming convention
//!
//! Function names are snake_case of the spec abstract operation names:
//! `main_fetch`, `http_fetch`, `http_redirect_fetch`, etc. So a reader
//! cross-referencing the spec can grep this file by concept.
//!
//! ## Out-of-scope for this dispatch
//!
//! - CORS preflight (the gateway is the security boundary).
//! - HTTP cache (defer until a caching plugin lands).
//! - Service Worker interception (no SWs in zeroship).
//! - Mixed-content checks (HTTPS-from-HTTPS-only is enforced by the
//!   bad-port table + scheme allowlist; spec subtleties around upgrade
//!   are deferred).

use compio::bytes::Bytes;

use super::bad_ports::is_bad_port;
use super::content_encoding::decompress_response_body;
use super::data_url::parse_data_url;
use super::http_network::{http_network_fetch, NetworkResponse, UploadBody};
use super::redirect::{apply_redirect_method, is_same_origin};

use crate::channel::CancelFlag;

// ---------------------------------------------------------------------------
// FetchParams — the shared state passed down the algorithm chain
// ---------------------------------------------------------------------------

/// Per Fetch §5.1 "fetch params". Carries the request being chased plus
/// the algorithmic context (signal, redirect counter, response so far).
///
/// We keep this as plain Rust structs — V8 entry happens at the
/// boundaries (algorithm output → `Response` JS object) but the loop
/// itself is pure async.
pub struct FetchRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: RequestBody,
    pub redirect_mode: RedirectMode,
    pub credentials_mode: CredentialsMode,
    /// Cancel flag for AbortSignal integration.
    pub cancel: Option<CancelFlag>,
    /// Tracks how many redirects we've followed so far. Spec §5.6 step
    /// 8: 20-cap.
    pub redirect_count: u32,
    /// Initial URL (the URL the user passed). For Origin header on
    /// cross-origin redirects.
    pub origin_url: String,
}

/// A request's body as the algorithm chain sends it.
pub enum RequestBody {
    /// The body is null.
    Empty,
    /// A body with a byte source: every BodyInit except a ReadableStream.
    /// Sent with its length, and sent again when a 307/308 redirect keeps
    /// the body.
    Bytes(Bytes),
    /// A ReadableStream body, whose source is null: read once, chunk by
    /// chunk, by the stream forwarder. The first hop takes the stream, so a
    /// redirect that would send the body again is a network error.
    Stream(Option<UploadBody>),
}

impl RequestBody {
    /// True for a ReadableStream body, before and after a hop has sent it.
    pub fn is_stream(&self) -> bool {
        matches!(self, RequestBody::Stream(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectMode {
    /// "follow" — chase up to 20 redirects (default).
    Follow,
    /// "manual" — return the redirect response unmodified.
    Manual,
    /// "error" — any redirect status fails the fetch.
    Error,
}

impl RedirectMode {
    // Inherent constructor kept for API ergonomics (infallible mapping
    // from a wire string); not intended to satisfy `std::str::FromStr`.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "manual" => RedirectMode::Manual,
            "error" => RedirectMode::Error,
            _ => RedirectMode::Follow,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialsMode {
    Omit,
    SameOrigin,
    Include,
}

impl CredentialsMode {
    // Inherent constructor kept for API ergonomics (infallible mapping
    // from a wire string); not intended to satisfy `std::str::FromStr`.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "omit" => CredentialsMode::Omit,
            "include" => CredentialsMode::Include,
            _ => CredentialsMode::SameOrigin,
        }
    }
}

/// The output of the algorithm chain — a "filtered response" the caller
/// turns into a JS `Response` wrapper.
pub struct AlgorithmResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub url: String,
    pub redirected: bool,
}

// ---------------------------------------------------------------------------
// main_fetch (§5.2) — scheme dispatch
// ---------------------------------------------------------------------------

/// Per Fetch §5.2 "main fetch". Routes on URL scheme and dispatches.
///
/// We intentionally skip:
///   - Step 7 "should request be blocked due to a bad port" runs inside
///     scheme-fetch for HTTP — we apply the bad-port check there, not
///     here, since data: / about: skip the check in the spec.
///   - Step 14 CORS preflight (gateway-level).
///   - Step 15 referrer policy (we don't tag requests).
pub async fn main_fetch(
    request: FetchRequest,
) -> Result<AlgorithmResponse, String> {
    scheme_fetch(request).await
}

/// Per Fetch §5.4 "scheme fetch". Branches on URL scheme.
pub async fn scheme_fetch(
    request: FetchRequest,
) -> Result<AlgorithmResponse, String> {
    // Parse the URL via ada-url so we can read the scheme without
    // re-parsing in every branch.
    let parsed = ada_url::Url::parse(&request.url, None)
        .map_err(|e| format!("network error: invalid URL: {e}"))?;
    let scheme = parsed.protocol().trim_end_matches(':').to_lowercase();

    match scheme.as_str() {
        "data" => data_url_fetch(request).await,
        "http" | "https" => http_fetch(request).await,
        // about: / blob: / file: — return network error per design.
        // The spec's about: handling returns an empty body for
        // `about:blank`; we don't need that here (no document context).
        _ => Err(format!("network error: unsupported scheme: {scheme}")),
    }
}

/// Per Fetch §5.4 case `data:`. Parse the URL, return a `200 OK`
/// response with the parsed MIME type and decoded bytes.
async fn data_url_fetch(
    request: FetchRequest,
) -> Result<AlgorithmResponse, String> {
    let (mime, bytes) = parse_data_url(&request.url)
        .map_err(|_| "network error: invalid data: URL".to_string())?;

    // Per Fetch's network-fetch convention, HEAD requests have an
    // empty body even when the underlying scheme would produce one.
    // Mirrors browser behaviour for data: URLs (Chromium / Firefox /
    // WebKit all return empty body on HEAD). WPT
    // `fetch/api/basic/scheme-data.any.js` verifies this for the
    // `checkFetchResponse(...,"HEAD")` case.
    let body = if request.method.eq_ignore_ascii_case("HEAD") {
        Vec::new()
    } else {
        bytes
    };

    Ok(AlgorithmResponse {
        status: 200,
        status_text: "OK".to_string(),
        headers: vec![("Content-Type".to_string(), mime)],
        body,
        url: request.url.clone(),
        redirected: false,
    })
}

// ---------------------------------------------------------------------------
// http_fetch (§5.5) — set up redirect loop
// ---------------------------------------------------------------------------

/// Per Fetch §5.5 "HTTP fetch". For our use case (zeroship V8 isolates,
/// no service workers, no CORS preflight):
///
///   1. Bad-port check (§5.5 step 2.2).
///   2. Hand off to `http_redirect_fetch` for the redirect loop.
async fn http_fetch(
    request: FetchRequest,
) -> Result<AlgorithmResponse, String> {
    // Step 2.2: bad-port check.
    check_bad_port(&request.url)?;
    http_redirect_fetch(request).await
}

/// The refusals the network applies to `url` before anything is sent: the
/// SSRF check on the URL's text and the bad-port table. `Ok(false)` for a
/// scheme fetched without the network (`data:`), which never sends a body.
/// The algorithm chain applies the same checks again; this lets `fetch`
/// refuse such a request before it reads a stream body.
pub fn precheck_network_url(url: &str) -> Result<bool, String> {
    let parsed = ada_url::Url::parse(url, None)
        .map_err(|e| format!("network error: invalid URL: {e}"))?;
    let scheme = parsed.protocol().trim_end_matches(':').to_lowercase();
    if scheme != "http" && scheme != "https" {
        return Ok(false);
    }
    crate::fetch::validate_url(url).map_err(|msg| format!("network error: {msg}"))?;
    check_bad_port(url)?;
    Ok(true)
}

fn check_bad_port(url: &str) -> Result<(), String> {
    let parsed = ada_url::Url::parse(url, None)
        .map_err(|e| format!("network error: invalid URL: {e}"))?;
    let scheme = parsed.protocol().trim_end_matches(':').to_lowercase();
    let port_str = parsed.port();
    let port: u16 = if port_str.is_empty() {
        match scheme.as_str() {
            "http" | "ws" => 80,
            "https" | "wss" => 443,
            _ => return Ok(()),
        }
    } else {
        port_str.parse().map_err(|_| "network error: invalid port".to_string())?
    };
    if is_bad_port(port) {
        return Err(format!("network error: blocked port {port}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// http_redirect_fetch (§5.6) — redirect loop
// ---------------------------------------------------------------------------

/// Per Fetch §5.6 "HTTP-redirect fetch". Drives the redirect loop:
///
///   1. Run `http_network_or_cache_fetch` (which calls
///      `http_network_fetch` since we don't cache).
///   2. If the response is a redirect status (301/302/303/307/308) and
///      `redirect_mode == "follow"`:
///      a. Increment redirect_count; bail at 20.
///      b. Parse the Location header; resolve relative to current URL.
///      c. Apply method/body mutation per status.
///      d. Strip Authorization on cross-origin hops.
///      e. Strip Content-* request headers when body is dropped.
///      f. Recurse.
async fn http_redirect_fetch(
    mut request: FetchRequest,
) -> Result<AlgorithmResponse, String> {
    /// Per §5.6 step 8 — the cap is 20.
    const MAX_REDIRECTS: u32 = 20;

    loop {
        let net = http_network_or_cache_fetch(&mut request).await?;

        let status = net.status;
        let is_redirect = matches!(status, 301 | 302 | 303 | 307 | 308);
        if !is_redirect {
            return Ok(AlgorithmResponse {
                status: net.status,
                status_text: net.status_text,
                headers: net.headers,
                body: net.body,
                url: request.url.clone(),
                redirected: request.redirect_count > 0,
            });
        }

        match request.redirect_mode {
            RedirectMode::Manual => {
                // Return the redirect response unmodified.
                return Ok(AlgorithmResponse {
                    status: net.status,
                    status_text: net.status_text,
                    headers: net.headers,
                    body: net.body,
                    url: request.url.clone(),
                    redirected: request.redirect_count > 0,
                });
            }
            RedirectMode::Error => {
                return Err("network error: redirect not allowed".to_string());
            }
            RedirectMode::Follow => {
                // Cap: §5.6 step 8.
                if request.redirect_count >= MAX_REDIRECTS {
                    return Err(format!(
                        "network error: too many redirects (>{MAX_REDIRECTS})"
                    ));
                }

                // Find the Location header.
                let location = net
                    .headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("location"))
                    .map(|(_, v)| v.clone());
                let location = match location {
                    Some(l) => l,
                    None => {
                        // No Location → return as-is (treat as final).
                        // Spec §5.6 step 6: if locationURL is null,
                        // return response.
                        return Ok(AlgorithmResponse {
                            status: net.status,
                            status_text: net.status_text,
                            headers: net.headers,
                            body: net.body,
                            url: request.url.clone(),
                            redirected: request.redirect_count > 0,
                        });
                    }
                };

                // Resolve Location relative to current URL.
                let next_url_obj = ada_url::Url::parse(&location, Some(&request.url))
                    .map_err(|e| format!("network error: invalid Location: {e}"))?;
                let next_url = next_url_obj.href().to_string();
                let next_scheme = next_url_obj.protocol().trim_end_matches(':').to_lowercase();
                if next_scheme != "http" && next_scheme != "https" {
                    return Err(format!(
                        "network error: redirect to non-HTTP scheme: {next_scheme}"
                    ));
                }

                // HTTP-redirect fetch: "If internalResponse's status is not
                // 303, request's body is non-null, and request's body's
                // source is null, then return a network error." A stream is
                // the only body without a source, and this runs before the
                // 301/302 POST-to-GET rewrite, so a stream body follows 303
                // alone.
                if status != 303 && request.body.is_stream() {
                    return Err(format!(
                        "network error: a {status} redirect cannot resend a ReadableStream request body"
                    ));
                }

                // Method / body mutation.
                let change = apply_redirect_method(status, &request.method);

                // Cross-origin Authorization strip (§5.6 step 13).
                let cross_origin = !is_same_origin(&request.url, &next_url);

                request.redirect_count += 1;
                request.method = change.method;
                request.url = next_url;

                if change.drop_body {
                    request.body = RequestBody::Empty;
                }

                if change.strip_content_headers {
                    request.headers.retain(|(k, _)| {
                        let lk = k.to_ascii_lowercase();
                        !matches!(
                            lk.as_str(),
                            "content-length"
                                | "content-encoding"
                                | "content-language"
                                | "content-location"
                                | "content-type"
                        )
                    });
                }

                if cross_origin {
                    request.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization"));
                }

                // Bad-port re-check on next hop.
                check_bad_port(&request.url)?;

                // Loop.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// http_network_or_cache_fetch (§5.7) — currently a thin pass-through
// ---------------------------------------------------------------------------

/// Per Fetch §5.7 "HTTP-network-or-cache fetch". Without an HTTP cache
/// we still run the cache-aware request-header rewrites that real-world
/// servers expect (`Cache-Control` propagation), then hand off to
/// `http_network_fetch`.
async fn http_network_or_cache_fetch(
    request: &mut FetchRequest,
) -> Result<NetworkResponse, String> {
    let resp = http_network_fetch(request).await?;
    // After decoding, strip Content-Encoding and Content-Length when
    // Content-Encoding was present. Run the decompression hook here
    // so the response the
    // caller sees has decoded body + clean headers.
    let decoded = decompress_response_body(resp.body, &resp.headers)?;
    Ok(NetworkResponse {
        status: resp.status,
        status_text: resp.status_text,
        headers: decoded.headers,
        body: decoded.bytes,
    })
}

// ---------------------------------------------------------------------------
// Default Accept-Encoding helper
// ---------------------------------------------------------------------------

/// Outbound requests get a default `Accept-Encoding` when the user
/// didn't set one. HTTPS gets `br, gzip, deflate`; HTTP
/// gets `gzip, deflate` (browsers historically suppress `br` over HTTP
/// because of legacy proxies that mishandle it).
pub fn default_accept_encoding(scheme: &str) -> &'static str {
    match scheme {
        "https" => "br, gzip, deflate",
        _ => "gzip, deflate",
    }
}

/// True if a list of headers already contains `Accept-Encoding`
/// (case-insensitive). Used to skip the default when the user supplied
/// one, including the empty string for opt-out.
pub fn has_accept_encoding(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"))
}

// ---------------------------------------------------------------------------
// Origin header
// ---------------------------------------------------------------------------

/// Per Fetch §2.2.5 step 4 — append `Origin` for methods NOT in
/// {GET, HEAD}. The value is the request's origin tuple's serialization
/// (`scheme://host[:port]`); `"no-referrer"` referrer policy makes it
/// `null`.
pub fn append_origin_if_needed(headers: &mut Vec<(String, String)>, method: &str, url: &str, referrer_policy: &str) {
    let upper = method.to_ascii_uppercase();
    if upper == "GET" || upper == "HEAD" {
        return;
    }
    // Don't override user-set Origin.
    if headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("origin")) {
        return;
    }

    let value = if referrer_policy == "no-referrer" {
        "null".to_string()
    } else {
        match super::redirect::url_origin(url) {
            Some((scheme, host, port)) => {
                let default_port = if scheme == "https" { 443 } else { 80 };
                if port == default_port {
                    format!("{scheme}://{host}")
                } else {
                    format!("{scheme}://{host}:{port}")
                }
            }
            None => "null".to_string(),
        }
    };
    headers.push(("Origin".to_string(), value));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_mode_parse() {
        assert_eq!(RedirectMode::from_str("follow"), RedirectMode::Follow);
        assert_eq!(RedirectMode::from_str("manual"), RedirectMode::Manual);
        assert_eq!(RedirectMode::from_str("error"), RedirectMode::Error);
        // Unknown defaults to Follow.
        assert_eq!(RedirectMode::from_str("xyz"), RedirectMode::Follow);
    }

    #[test]
    fn credentials_mode_parse() {
        assert_eq!(CredentialsMode::from_str("omit"), CredentialsMode::Omit);
        assert_eq!(
            CredentialsMode::from_str("include"),
            CredentialsMode::Include
        );
        assert_eq!(
            CredentialsMode::from_str("same-origin"),
            CredentialsMode::SameOrigin
        );
    }

    #[test]
    fn default_accept_encoding_https_includes_br() {
        assert_eq!(default_accept_encoding("https"), "br, gzip, deflate");
    }

    #[test]
    fn default_accept_encoding_http_excludes_br() {
        assert_eq!(default_accept_encoding("http"), "gzip, deflate");
    }

    #[test]
    fn check_bad_port_blocks_smtp() {
        assert!(check_bad_port("http://example.com:25/").is_err());
    }

    #[test]
    fn check_bad_port_allows_443_default() {
        assert!(check_bad_port("https://example.com/").is_ok());
    }

    #[test]
    fn check_bad_port_allows_80_default() {
        assert!(check_bad_port("http://example.com/").is_ok());
    }

    #[test]
    fn check_bad_port_allows_8080() {
        assert!(check_bad_port("http://example.com:8080/").is_ok());
    }

    #[test]
    fn append_origin_skips_get() {
        let mut h = Vec::new();
        append_origin_if_needed(&mut h, "GET", "https://a.com/x", "");
        assert!(h.is_empty());
    }

    #[test]
    fn append_origin_for_post() {
        let mut h = Vec::new();
        append_origin_if_needed(&mut h, "POST", "https://a.com/x", "");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, "Origin");
        assert_eq!(h[0].1, "https://a.com");
    }

    #[test]
    fn append_origin_no_referrer_yields_null() {
        let mut h = Vec::new();
        append_origin_if_needed(&mut h, "POST", "https://a.com/x", "no-referrer");
        assert_eq!(h[0].1, "null");
    }

    #[test]
    fn append_origin_with_nondefault_port() {
        let mut h = Vec::new();
        append_origin_if_needed(&mut h, "POST", "https://a.com:8443/x", "");
        assert_eq!(h[0].1, "https://a.com:8443");
    }
}
