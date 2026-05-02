//! HTTP network fetch — the cyper bridge.
//!
//! Per Fetch §5.7 / §5.8 (HTTP-network-or-cache fetch / HTTP-network
//! fetch). For our simplified flow:
//!
//!   1. Validate the URL through SSRF (preserving the existing
//!      `validate_url` from `crate::fetch`).
//!   2. Build a `cyper::RequestBuilder` from the FetchRequest.
//!   3. Drain the response body fully (subject to MAX_RESPONSE_SIZE),
//!      since the algorithms layer needs raw bytes for redirect
//!      response handling and Content-Encoding decoding.
//!
//! The streaming-response path the legacy fetch.rs uses (chunk-by-chunk
//! into a JS ReadableStream via stream_id) is preserved for non-redirected
//! final hops in a follow-up — for v1 native fetch we buffer to keep the
//! algorithm chain straightforward and avoid the bridge through V8 mid-
//! request.

use super::algorithms::{
    append_origin_if_needed, default_accept_encoding, has_accept_encoding, FetchRequest,
};

use crate::fetch::MAX_RESPONSE_SIZE;

/// Network response surfaced to the algorithm chain. Headers come back
/// as Vec<(name, value)> so the chain can mutate them (e.g. strip
/// Content-Encoding after decompression).
#[derive(Debug)]
pub struct NetworkResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Per Fetch §5.8 "HTTP-network fetch". Performs the actual TCP/TLS
/// round-trip via cyper.
pub async fn http_network_fetch(
    request: &FetchRequest,
) -> Result<NetworkResponse, String> {
    // SSRF — validate URL. The cyper resolver wraps `is_blocked_ip` for
    // DNS-level filtering; the string-level fast path catches literal
    // private IPs early.
    if let Err(msg) = crate::fetch::validate_url(&request.url) {
        return Err(format!("network error: {msg}"));
    }

    // Cancellation: pre-send check.
    if let Some(flag) = &request.cancel {
        if flag.is_cancelled() {
            return Err("network error: aborted".to_string());
        }
    }

    // Build the cyper Client (per-thread pool with SsrfResolver).
    let client = shared_cyper_client();

    let method = parse_http_method(&request.method)?;
    let mut builder = client
        .request(method, &request.url)
        .map_err(|e| format!("network error: {e}"))?;

    // Headers — clone the request headers, then layer on our defaults.
    let mut req_headers = request.headers.clone();

    // Default Accept-Encoding (D-15).
    if !has_accept_encoding(&req_headers) {
        let scheme = ada_url::Url::parse(&request.url, None)
            .map(|u| u.protocol().trim_end_matches(':').to_lowercase())
            .unwrap_or_else(|_| "https".to_string());
        let ae = default_accept_encoding(&scheme);
        req_headers.push(("Accept-Encoding".to_string(), ae.to_string()));
    } else {
        // User supplied AE; honor it. If it's the empty string, that's
        // an opt-out — drop the header so cyper doesn't send a trailing
        // empty header.
        req_headers.retain(|(k, v)| {
            !(k.eq_ignore_ascii_case("accept-encoding") && v.is_empty())
        });
    }

    // Origin header (D-16).
    append_origin_if_needed(&mut req_headers, &request.method, &request.url, "");

    for (k, v) in &req_headers {
        builder = builder
            .header(k.as_str(), v.as_str())
            .map_err(|e| format!("network error: invalid header {k}: {e}"))?;
    }

    // Body — pass through if rewindable bytes are available. For
    // BodySource::Stream the algorithm chain's redirect step would have
    // erred earlier on a 307/308; on a non-redirect first hop we have
    // the buffered bytes in `body`.
    if let Some(bytes) = &request.body {
        builder = builder.body(bytes.clone());
    }

    // Re-check cancellation right before sending.
    if let Some(flag) = &request.cancel {
        if flag.is_cancelled() {
            return Err("network error: aborted".to_string());
        }
    }

    // Send. cyper does not auto-follow redirects; that's our job.
    let response = builder
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;

    if let Some(flag) = &request.cancel {
        if flag.is_cancelled() {
            return Err("network error: aborted".to_string());
        }
    }

    let status = response.status().as_u16();
    let status_text = response
        .status()
        .canonical_reason()
        .unwrap_or("")
        .to_string();

    // Headers — preserve all of them (the algorithm chain decides what
    // to strip).
    let mut headers: Vec<(String, String)> = Vec::new();
    for (k, v) in response.headers() {
        if let Ok(s) = v.to_str() {
            headers.push((k.to_string(), s.to_string()));
        }
    }

    // Pre-flight Content-Length cap (legacy preflight from fetch.rs).
    if let Some(len) = response.content_length() {
        if len > MAX_RESPONSE_SIZE as u64 {
            return Err(format!(
                "network error: response too large ({len} > {MAX_RESPONSE_SIZE})"
            ));
        }
    }

    let body_bytes = response
        .bytes()
        .await
        .map_err(|e| format!("network error: body read failed: {e}"))?;
    if body_bytes.len() > MAX_RESPONSE_SIZE {
        return Err(format!(
            "network error: response body exceeded {MAX_RESPONSE_SIZE}"
        ));
    }

    Ok(NetworkResponse {
        status,
        status_text,
        headers,
        body: body_bytes.to_vec(),
    })
}

fn parse_http_method(s: &str) -> Result<http::Method, String> {
    match s.to_ascii_uppercase().as_str() {
        "GET" => Ok(http::Method::GET),
        "POST" => Ok(http::Method::POST),
        "PUT" => Ok(http::Method::PUT),
        "DELETE" => Ok(http::Method::DELETE),
        "HEAD" => Ok(http::Method::HEAD),
        "OPTIONS" => Ok(http::Method::OPTIONS),
        "PATCH" => Ok(http::Method::PATCH),
        other => http::Method::from_bytes(other.as_bytes())
            .map_err(|e| format!("invalid HTTP method '{other}': {e}")),
    }
}

/// Reuse the per-thread cyper client from `crate::fetch`. The legacy
/// `__rawFetch` path uses the same constant so client-side connection
/// pooling is shared across the polyfill cutover window.
fn shared_cyper_client() -> cyper::Client {
    // Build with the same SsrfResolver as the legacy path. We mirror
    // the thread_local construction here rather than reaching into
    // `crate::fetch::CLIENT` (which is not pub) — this keeps the module
    // boundary clean.
    thread_local! {
        static CLIENT: cyper::Client = {
            let builder = cyper::Client::builder();
            if std::env::var("ZEROSHIP_DEV").is_ok() {
                builder.build()
            } else {
                builder.custom_resolver(crate::fetch::SsrfResolver).build()
            }
        };
    }
    CLIENT.with(|c| c.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_method_get() {
        assert_eq!(parse_http_method("get").unwrap(), http::Method::GET);
        assert_eq!(parse_http_method("GET").unwrap(), http::Method::GET);
    }

    #[test]
    fn parse_method_custom() {
        assert_eq!(parse_http_method("PROPFIND").unwrap().as_str(), "PROPFIND");
    }

    #[test]
    fn parse_method_invalid() {
        assert!(parse_http_method("BAD METHOD").is_err());
    }
}
