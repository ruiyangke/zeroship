//! HTTP network fetch — the cyper bridge.
//!
//! Per Fetch §5.7 / §5.8 (HTTP-network-or-cache fetch / HTTP-network
//! fetch). For our simplified flow:
//!
//!   1. Validate the URL through SSRF (`crate::fetch::validate_url` plus
//!      `crate::fetch::SsrfResolver` wired into the cyper client).
//!   2. Build a `cyper::RequestBuilder` from the FetchRequest. A byte body
//!      is sent with its length; a ReadableStream body is sent as it is
//!      read, chunk by chunk, with the stream forwarder's backpressure.
//!   3. Drain the response body fully (subject to MAX_RESPONSE_SIZE),
//!      since the algorithms layer needs raw bytes for redirect
//!      response handling and Content-Encoding decoding.
//!
//! An abort is raced against every await: sending the request (which
//! includes the whole of a streamed body) and reading the response.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use compio::bytes::Bytes;

use super::algorithms::{
    append_origin_if_needed, default_accept_encoding, has_accept_encoding, FetchRequest,
    RequestBody,
};

use crate::channel::CancelFlag;
use crate::fetch::MAX_RESPONSE_SIZE;
use crate::streams::stream_forwarder::{UploadControl, UploadReader};
use futures::{FutureExt, Stream, StreamExt, pin_mut};

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
    request: &mut FetchRequest,
) -> Result<NetworkResponse, String> {
    // SSRF — validate URL. The cyper resolver wraps `is_blocked_ip` for
    // DNS-level filtering; the string-level fast path catches literal
    // private IPs early.
    if let Err(msg) = crate::fetch::validate_url(&request.url) {
        return Err(format!("network error: {msg}"));
    }

    // Cancellation: pre-send check.
    if let Some(flag) = &request.cancel
        && flag.is_cancelled()
    {
        return Err("network error: aborted".to_string());
    }

    // Build the cyper Client (per-thread pool with SsrfResolver).
    let client = crate::transport::client::shared_cyper_client();

    let method = parse_http_method(&request.method)?;
    let mut builder = client
        .request(method, &request.url)
        .map_err(|e| format!("network error: {e}"))?;

    // Headers — clone the request headers, then layer on our defaults.
    let mut req_headers = request.headers.clone();

    // Default Accept-Encoding.
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

    // Origin header.
    append_origin_if_needed(&mut req_headers, &request.method, &request.url, "");

    // The body is framed once, by the client: a creator's Transfer-Encoding
    // (a forbidden request header in Fetch) is dropped, so it can never ride
    // alongside a Content-Length. A creator's Content-Length is kept and
    // must match the bytes sent.
    req_headers.retain(|(k, _)| !k.eq_ignore_ascii_case("transfer-encoding"));

    for (k, v) in &req_headers {
        builder = builder
            .header(k.as_str(), v.as_str())
            .map_err(|e| format!("network error: invalid header {k}: {e}"))?;
    }

    // Body. A byte body is sent again on each hop that keeps it; a stream
    // body is taken by the hop that sends it.
    let mut upload_failure = None;
    let mut upload_control = None;
    match &mut request.body {
        RequestBody::Empty => {}
        RequestBody::Bytes(bytes) => builder = builder.body(bytes.clone()),
        RequestBody::Stream(slot) => {
            let upload = slot
                .take()
                .ok_or_else(|| "network error: the ReadableStream request body was already sent".to_string())?;
            upload_failure = Some(upload.failure.clone());
            upload_control = Some(upload.reader.control());
            builder = builder.body(cyper::Body::stream(send_wrapper::SendWrapper::new(upload)));
        }
    }
    // However this hop ends, a streamed body it has not finished sending
    // stops with it: an aborted or failed hop, and a response that is
    // complete while the body is not (the server answered without reading
    // the rest, and the HTTP client may keep the connection open and go on
    // sending into it).
    let _end_upload = upload_control.map(EndUpload);

    // Re-check cancellation right before sending.
    if let Some(flag) = &request.cancel
        && flag.is_cancelled()
    {
        return Err("network error: aborted".to_string());
    }

    // Send. cyper does not auto-follow redirects; that's our job. A stream
    // body is sent inside this await, so an abort must be able to end it.
    let sent = until_cancelled(request.cancel.as_ref(), builder.send())
        .await
        .ok_or_else(|| "network error: aborted".to_string())?;
    let response = sent.map_err(|e| {
        // A failed stream body is the cause; the client's error only says
        // the body ended early.
        match upload_failure.as_ref().and_then(|f| f.borrow().clone()) {
            Some(reason) => format!("network error: request body {reason}"),
            None => format!("network error: {e}"),
        }
    })?;

    if let Some(flag) = &request.cancel
        && flag.is_cancelled()
    {
        return Err("network error: aborted".to_string());
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
    if let Some(len) = response.content_length()
        && len > MAX_RESPONSE_SIZE as u64
    {
        return Err(format!(
            "network error: response too large ({len} > {MAX_RESPONSE_SIZE})"
        ));
    }

    let mut body = Vec::new();
    let body_stream = response.bytes_stream();
    pin_mut!(body_stream);
    loop {
        let maybe_chunk = until_cancelled(request.cancel.as_ref(), body_stream.next())
            .await
            .ok_or_else(|| "network error: aborted".to_string())?;

        let Some(chunk) = maybe_chunk else { break };
        let chunk = chunk.map_err(|e| format!("network error: body read failed: {e}"))?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_SIZE {
            return Err(format!("network error: response body exceeded {MAX_RESPONSE_SIZE}"));
        }
        body.extend_from_slice(&chunk);
        if let Some(flag) = &request.cancel
            && flag.is_cancelled()
        {
            return Err("network error: aborted".to_string());
        }
    }

    Ok(NetworkResponse {
        status,
        status_text,
        headers,
        body,
    })
}

/// Await `work` unless `cancel` fires first; `None` means it was cancelled.
async fn until_cancelled<F: Future>(cancel: Option<&CancelFlag>, work: F) -> Option<F::Output> {
    let Some(flag) = cancel else {
        return Some(work.await);
    };
    let work = work.fuse();
    pin_mut!(work);
    let cancelled = futures::future::poll_fn(|cx| {
        if flag.is_cancelled() {
            Poll::Ready(())
        } else {
            flag.register_waker(cx.waker());
            Poll::Pending
        }
    })
    .fuse();
    pin_mut!(cancelled);
    futures::select! {
        output = work => Some(output),
        _ = cancelled => None,
    }
}

/// A ReadableStream request body as the HTTP client consumes it.
///
/// The stream forwarder's [`UploadReader`], yielding chunks in order. A
/// failed source ends the body with an error (the request fails rather than
/// sending a truncated body as complete), and the reason is kept for the
/// fetch's rejection. Dropping it before the end cancels the source.
pub struct UploadBody {
    reader: UploadReader,
    failure: Rc<RefCell<Option<String>>>,
    /// The fetch's in-flight slot, held until the client finishes or drops
    /// the body.
    slot: Option<Rc<super::FetchSlot>>,
}

impl UploadBody {
    pub fn new(reader: UploadReader) -> Self {
        Self { reader, failure: Rc::new(RefCell::new(None)), slot: None }
    }

    /// Hold `slot` for as long as the body lives.
    pub fn hold_slot(&mut self, slot: Rc<super::FetchSlot>) {
        self.slot = Some(slot);
    }
}

/// Cancels a streamed body when the hop that sent it ends.
struct EndUpload(UploadControl);

impl Drop for EndUpload {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Stream for UploadBody {
    type Item = Result<Bytes, cyper::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            return match this.reader.poll_next_chunk(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(None) => {
                    this.slot = None;
                    Poll::Ready(None)
                }
                // An empty chunk carries no bytes: nothing to hand the client.
                Poll::Ready(Some(Ok(chunk))) if chunk.is_empty() => continue,
                Poll::Ready(Some(Ok(chunk))) => Poll::Ready(Some(Ok(chunk))),
                Poll::Ready(Some(Err(err))) => {
                    let reason = err.to_string();
                    *this.failure.borrow_mut() = Some(reason.clone());
                    Poll::Ready(Some(Err(cyper::Error::System(std::io::Error::other(reason)))))
                }
            };
        }
    }
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

// `shared_cyper_client` lives in `crate::transport::client` - the
// thread-local Client (a per-THREAD connection pool carrying the SSRF
// resolver, shared across every app resident on that thread, NOT per
// isolate) is shared between `web::fetch` and any future direct transport
// callers.

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
