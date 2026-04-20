//! Complete fetch lifecycle — V8 callback + cyper HTTP execution.
//!
//! 1. `raw_fetch_callback`: V8 callback, pushes FetchRequest into state
//! 2. `execute_fetch`: cyper HTTP client execution (called by runtime.rs pump)
//! 3. SSRF protection: string-level `validate_url` + DNS-level `SsrfResolver`
//! 4. `parse_headers` — JSON → header pairs
//!
//! Note: cyper 0.8 does **not** follow HTTP redirects automatically, so every
//! connection goes through our custom resolver, and `Response::url` always
//! matches the requested URL. If redirect-following is ever enabled, it must
//! be done manually with `validate_url` called on each hop.

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;

use cyper::resolve::Resolve;
use futures::Stream;
use futures::stream;
use http::Uri;

use crate::state::{FetchRequest, OpResult, SharedState};

/// Maximum response body size: 10 MB.
pub const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024;

/// Build an error JSON string using serde_json.
pub fn error_json(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

/// True for IP addresses that must never be reachable from user fetch code.
///
/// Centralises the blocklist used by both the string-level `validate_url`
/// fast path (rejects literal IPs) and the DNS-level `SsrfResolver` (rejects
/// hostnames whose A/AAAA records point into these ranges).
#[must_use]
pub fn is_blocked_ip(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            v4.is_loopback()                       // 127.0.0.0/8
                || v4.is_private()                 // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local()              // 169.254/16
                || v4.is_unspecified()             // 0.0.0.0
                || v4.is_broadcast()               // 255.255.255.255
                || v4.is_multicast()               // 224.0.0.0/4
                || v4.is_documentation()           // 192.0.2/24, 198.51.100/24, 203.0.113/24
                || v4.octets()[0] == 0             // 0.0.0.0/8 — "this network"
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64  // 100.64/10 CGNAT
                || v4.octets()[0] == 192 && v4.octets()[1] == 0 && v4.octets()[2] == 0 // 192.0.0/24
                || v4.octets()[0] == 198 && (v4.octets()[1] & 0xFE) == 18  // 198.18/15 benchmarking
                || v4.octets()[0] >= 240           // 240.0.0.0/4 reserved + 255.255.255.255
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()                       // ::1
                || v6.is_unspecified()             // ::
                || v6.is_multicast()               // ff00::/8
                || (v6.segments()[0] & 0xffc0) == 0xfe80   // fe80::/10 link-local
                || (v6.segments()[0] & 0xfe00) == 0xfc00   // fc00::/7 unique-local
                || v6.segments()[..5] == [0, 0, 0, 0, 0] && v6.segments()[5] == 0xffff // ::ffff:0:0/96 v4-mapped
                || v6.segments()[0] == 0x2001 && v6.segments()[1] == 0xdb8 // 2001:db8::/32 documentation
                || v6.segments()[0] == 0x2001 && (v6.segments()[1] & 0xff00) == 0x0200 // 2001:2::/48 benchmarking
        }
    }
}

/// Validate the URL to prevent SSRF attacks (string-level fast path).
///
/// Blocks non-HTTP(S) schemes and literal private/loopback/link-local/etc IPs
/// embedded in the URL. A second layer of protection runs at DNS resolution
/// time via `SsrfResolver` — domain names that resolve into blocked ranges
/// are rejected there, since this function cannot see them.
///
/// In dev mode (`ZEROSHIP_DEV=1`), localhost/loopback is allowed so the
/// Vite plugin's ModuleRunner can fetch modules from the Vite dev server.
pub fn validate_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL: {e}"))?;

    // Only allow http and https schemes
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => return Err(format!("Blocked URL scheme: {scheme}")),
    }

    // In dev mode, skip host/IP validation (allows localhost fetch to Vite)
    if std::env::var("ZEROSHIP_DEV").is_ok() {
        return Ok(());
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?
        .to_lowercase();

    // Block localhost
    if host == "localhost" {
        return Err("Blocked request to localhost".to_string());
    }

    // Try to parse as IP address (handles both bare IPs and bracket-stripped IPv6)
    let ip: Option<IpAddr> = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok();

    if let Some(addr) = ip
        && is_blocked_ip(addr)
    {
        return Err(format!("Blocked request to private/internal IP: {addr}"));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// SsrfResolver — DNS resolver that filters out private/loopback addresses
// ---------------------------------------------------------------------------

/// Custom cyper resolver. Performs the same work as the default (std DNS
/// lookup) then strips every `IpAddr` that `is_blocked_ip` rejects. If the
/// remaining set is empty, returns an error so cyper fails the connection.
///
/// This closes the SSRF hole where a public hostname resolves to an RFC1918
/// address — the caller sees a generic connect error instead of reaching the
/// internal service.
pub struct SsrfResolver;

impl Resolve for SsrfResolver {
    type Err = std::io::Error;

    async fn resolve(&self, uri: &Uri) -> Result<impl Stream<Item = IpAddr> + '_, Self::Err> {
        use std::io::{Error, ErrorKind};

        let host = uri
            .host()
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "URI missing host"))?;
        let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
            Some("https") => 443,
            _ => 80,
        });

        // Strip IPv6 literal brackets before handing to to_socket_addrs
        let host_clean = host.trim_start_matches('[').trim_end_matches(']');
        let target = format!("{host_clean}:{port}");

        // std DNS resolution runs on the current thread and blocks briefly;
        // acceptable for fetch since this happens once per request.
        let addrs: Vec<IpAddr> = std::net::ToSocketAddrs::to_socket_addrs(&target)?
            .map(|sa| sa.ip())
            .filter(|ip| !is_blocked_ip(*ip))
            .collect();

        if addrs.is_empty() {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "all resolved addresses are in blocked ranges (SSRF guard)",
            ));
        }

        Ok(stream::iter(addrs))
    }
}

/// Parse headers from JSON — supports both `[["key","val"],...]` and `{"key":"val",...}` formats.
pub fn parse_headers(json: &str) -> Result<Vec<(String, String)>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("JSON parse error: {e}"))?;

    match value {
        serde_json::Value::Array(arr) => {
            let mut headers = Vec::new();
            for item in arr {
                match item {
                    serde_json::Value::Array(pair) if pair.len() == 2 => {
                        let key = pair[0].as_str().ok_or("Header key must be a string")?;
                        let val = pair[1].as_str().ok_or("Header value must be a string")?;
                        headers.push((key.to_string(), val.to_string()));
                    }
                    _ => return Err("Header array entries must be [key, value] pairs".to_string()),
                }
            }
            Ok(headers)
        }
        serde_json::Value::Object(map) => {
            let mut headers = Vec::new();
            for (key, val) in map {
                let val_str = val.as_str().ok_or("Header value must be a string")?;
                headers.push((key, val_str.to_string()));
            }
            Ok(headers)
        }
        _ => Err("Headers must be an array or object".to_string()),
    }
}

/// Hand-written V8 callback for `__rawFetch(method, url, headersJson, body)`.
///
/// Creates a Promise, allocates an op-id, and pushes a `FetchRequest` into
/// `state.spawned_fetches`. The runtime executor drains these and spawns the
/// actual HTTP I/O.
pub fn raw_fetch_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    // Extract JS arguments
    let method: String = args.get(0).to_rust_string_lossy(scope);
    let url: String = args.get(1).to_rust_string_lossy(scope);
    let headers_json: String = args.get(2).to_rust_string_lossy(scope);
    let body: Option<String> = if args.length() > 3 && !args.get(3).is_null_or_undefined() {
        Some(args.get(3).to_rust_string_lossy(scope))
    } else {
        None
    };

    // Create promise
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);

    // Admission control. Two caps, both per-runtime (≅ per-app, since the
    // worker gives each app its own Runtime):
    //
    //   1. MAX_PENDING_OPS (1024): total async ops in flight — bounds the
    //      resolver-map memory and the pump's `pending_ops` length.
    //   2. MAX_PENDING_FETCHES (64): concurrent outbound HTTP. Tighter,
    //      because fetch shares the process-wide cyper connection pool
    //      with every other app in this worker — without this, one app's
    //      runaway fetch loop saturates that pool for its neighbours.
    //
    // Either cap triggering yields a RangeError so the JS code sees a
    // synchronous, catchable rejection rather than an eventual timeout.
    {
        let s = state.borrow();
        let in_flight_ops = s.pending_resolvers.len()
            + s.spawned_fetches.len()
            + s.spawned_ops.len();

        let (over_cap, err_msg) = if in_flight_ops >= crate::state::MAX_PENDING_OPS {
            (true, format!(
                "Too many concurrent async operations (limit: {})",
                crate::state::MAX_PENDING_OPS
            ))
        } else if s.in_flight_fetches >= crate::state::MAX_PENDING_FETCHES {
            (true, format!(
                "Too many concurrent fetches (limit: {})",
                crate::state::MAX_PENDING_FETCHES
            ))
        } else {
            (false, String::new())
        };

        if over_cap {
            drop(s);
            let err_str = v8::String::new(scope, &err_msg).unwrap();
            let err = v8::Exception::range_error(scope, err_str);
            resolver.reject(scope, err);
            rv.set(promise.into());
            return;
        }
    }

    let global_resolver = v8::Global::new(scope, resolver);

    // Allocate op_id + stream_id, capture request context
    let (op_id, stream_id, request_id, cancel) = {
        let mut s = state.borrow_mut();
        let id = s.next_op_id;
        s.next_op_id += 1;
        s.pending_resolvers.insert(id, global_resolver);

        let sid = s.alloc_stream_id();

        let req_id = s.executing_request_id;
        let cancel = s.executing_request_cancel.clone();

        (id, sid, req_id, cancel)
    };

    // Queue the fetch request for the runtime executor and bump the
    // fetch concurrency counter; `spawn_body_reader` decrements it when
    // the body drains. Must happen under the same borrow so the counter
    // can't race with the admission check above.
    state.borrow_mut().in_flight_fetches += 1;
    state.borrow_mut().spawned_fetches.push(FetchRequest {
        op_id,
        stream_id,
        request_id,
        method,
        url,
        headers_json,
        body,
        cancel,
    });

    rv.set(promise.into());
}

// ===========================================================================
// cyper-based fetch execution (absorbed from io/fetch.rs)
// ===========================================================================

/// Shared cyper Client — reuses connections across requests.
/// cyper::Client is Arc-based and Send+Sync; the underlying CompioExecutor
/// dispatches work to whichever compio runtime is current on the calling thread.
///
/// The client is configured with `SsrfResolver`, which drops every resolved
/// IP address that is in a non-public range before cyper attempts to connect.
/// In dev mode (`ZEROSHIP_DEV=1`), a default resolver is used so loopback
/// targets (Vite dev server, integration tests) can be reached.
fn shared_client() -> &'static cyper::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<cyper::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let builder = cyper::Client::builder();
        if std::env::var("ZEROSHIP_DEV").is_ok() {
            builder.build()
        } else {
            builder.custom_resolver(SsrfResolver).build()
        }
    })
}

/// Execute a `FetchRequest` on the compio event loop via cyper.
/// Returns a future that resolves to an `OpResult` carrying the **response
/// headers only**; the body streams in through separate `OpResult::StreamChunk`
/// events driven by a detached body-reader task.
///
/// Streaming rationale: the old path called `response.bytes().await`, which
/// held the entire response in memory before resolving the JS Promise. For
/// LLM streaming, large downloads, or slow upstream endpoints this was a
/// latency AND memory problem. The JS side (`fetch.js`) already understood
/// `{ stream_id }` responses — it just never received one — so we reuse that
/// path: the Rust side hands JS a ReadableStream wrapping `stream_id`, then
/// the body reader pushes chunks into it as they arrive.
///
/// Cancellation: the fetch checks `cancel` before sending, and the body
/// reader checks it between chunks. If the owning request has been
/// cancelled, the reader closes the stream and stops pulling bytes so
/// we don't burn upstream bandwidth on responses nobody's reading.
pub(crate) fn execute_fetch(
    req: FetchRequest,
    state: SharedState,
) -> Pin<Box<dyn Future<Output = OpResult>>> {
    let FetchRequest {
        op_id, stream_id, request_id, method, url, headers_json, body, cancel,
    } = req;

    Box::pin(async move {
        // Fast-path cancellation: if the owning request was already cancelled
        // before the pump dequeued this fetch, don't even open a socket.
        // Must release the fetch-concurrency slot we reserved in the V8
        // callback before returning — otherwise cancelled fetches poison
        // the MAX_PENDING_FETCHES budget for the lifetime of the runtime.
        if let Some(flag) = &cancel
            && flag.is_cancelled()
        {
            release_fetch_slot(&state);
            return OpResult::Completed {
                op_id,
                value: error_json("fetch aborted: request cancelled"),
                request_id,
            };
        }

        let result = send_and_stream_response(
            &method,
            &url,
            &headers_json,
            body.as_deref(),
            cancel.clone(),
            stream_id,
            request_id,
            state.clone(),
        )
        .await;

        // Error paths (URL rejected, connect failed, pre-body cancellation)
        // never spawn the body reader, so they own the slot release. The
        // success path hands ownership of the slot to `spawn_body_reader`,
        // which releases on EOF / error / cancel — see there.
        let value = match result {
            Ok(json) => json,
            Err(err_json) => {
                release_fetch_slot(&state);
                err_json
            }
        };
        OpResult::Completed { op_id, value, request_id }
    })
}

/// Decrement the per-runtime in-flight-fetch counter. Safe to call under
/// a fresh borrow of state — RefCell is single-threaded and the fetch
/// paths that call this never hold an outer borrow across the `.await`
/// that leads here.
fn release_fetch_slot(state: &SharedState) {
    let mut s = state.borrow_mut();
    s.in_flight_fetches = s.in_flight_fetches.saturating_sub(1);
}

// ---------------------------------------------------------------------------
// Streaming response helpers
// ---------------------------------------------------------------------------

/// Send the outbound request, return the header JSON (for the fetch Promise
/// resolve), and detach a task that streams body chunks to `stream_id`.
async fn send_and_stream_response(
    method: &str,
    url: &str,
    headers_json: &str,
    body: Option<&str>,
    cancel: Option<crate::channel::CancelFlag>,
    stream_id: u32,
    request_id: Option<u64>,
    state: SharedState,
) -> Result<String, String> {
    if let Err(msg) = validate_url(url) {
        return Err(error_json(&msg));
    }

    let client = shared_client();

    let http_method = match method.to_uppercase().as_str() {
        "GET" => http::Method::GET,
        "POST" => http::Method::POST,
        "PUT" => http::Method::PUT,
        "DELETE" => http::Method::DELETE,
        "PATCH" => http::Method::PATCH,
        "HEAD" => http::Method::HEAD,
        "OPTIONS" => http::Method::OPTIONS,
        other => http::Method::from_bytes(other.as_bytes())
            .map_err(|e| error_json(&format!("Invalid HTTP method: {e}")))?,
    };

    let mut builder = client.request(http_method, url)
        .map_err(|e| error_json(&e.to_string()))?;

    if !headers_json.is_empty() {
        match parse_headers(headers_json) {
            Ok(headers) => {
                for (key, value) in headers {
                    builder = builder.header(&key, &value)
                        .map_err(|e| error_json(&format!("Invalid header: {e}")))?;
                }
            }
            Err(e) => return Err(error_json(&format!("Invalid headers: {e}"))),
        }
    }

    if let Some(body) = body {
        builder = builder.body(body.to_string());
    }

    // Re-check cancellation right before sending — the request may have
    // been cancelled while we were parsing headers / validating the URL.
    if let Some(flag) = &cancel
        && flag.is_cancelled()
    {
        return Err(error_json("fetch aborted: request cancelled"));
    }

    let response = builder.send().await
        .map_err(|e| error_json(&e.to_string()))?;

    // If cancelled between send completing and headers parsing, drop
    // everything without touching the body.
    if let Some(flag) = &cancel
        && flag.is_cancelled()
    {
        return Err(error_json("fetch aborted: request cancelled"));
    }

    let status = response.status().as_u16();
    let status_text = response.status().canonical_reason().unwrap_or("").to_string();
    let final_url = response.url().to_string();
    let redirected = final_url != url;

    let mut resp_headers: Vec<(String, String)> = Vec::new();
    for (key, value) in response.headers() {
        if let Ok(v) = value.to_str() {
            resp_headers.push((key.to_string(), v.to_string()));
        }
    }

    // Preflight check: if the server told us up front that the body is
    // larger than the cap, reject here so we don't even start streaming.
    if let Some(len) = response.content_length()
        && len > MAX_RESPONSE_SIZE as u64
    {
        return Err(error_json(&format!(
            "Response too large: {len} bytes (max {MAX_RESPONSE_SIZE})"
        )));
    }

    // Spawn a detached reader that streams body chunks into `stream_id`.
    // It runs until the body is fully read, the byte cap is exceeded, or
    // the owning request is cancelled — whichever comes first.
    spawn_body_reader(response, stream_id, request_id, cancel, state);

    // Resolve the JS fetch Promise immediately with the header-only JSON.
    // `stream_id` tells `fetch.js` to construct a ReadableStream around it
    // instead of expecting a full `body` string.
    Ok(serde_json::json!({
        "status": status,
        "statusText": status_text,
        "headers": resp_headers,
        "stream_id": stream_id,
        "url": final_url,
        "redirected": redirected,
    })
    .to_string())
}

/// Read the cyper response body and push each chunk into `state.spawned_ops`
/// as an `OpResult::StreamChunk`. Runs as a detached compio task so the
/// fetch Promise can resolve on headers without waiting for the body.
fn spawn_body_reader(
    response: cyper::Response,
    stream_id: u32,
    _request_id: Option<u64>,
    cancel: Option<crate::channel::CancelFlag>,
    state: SharedState,
) {
    use futures::StreamExt;

    compio::runtime::spawn(async move {
        crate::panic_util::guard("spawn_body_reader", async move {
            let mut body_stream = response.bytes_stream();
            let mut total = 0usize;

            // Every termination path (normal EOF, upstream error, byte-cap
            // overflow, cancellation) breaks out of the loop to a single
            // release-site below. Earlier revisions had per-arm early returns
            // and forgot the slot release on two of them — consolidating is
            // cheaper than proving correctness on every arm.
            loop {
                if let Some(flag) = &cancel
                    && flag.is_cancelled()
                {
                    push_stream_chunk_to_state(&state, stream_id, Vec::new(), true);
                    break;
                }

                let chunk = match body_stream.next().await {
                    Some(Ok(b)) => b,
                    Some(Err(e)) => {
                        eprintln!("[fetch] body read error on stream {stream_id}: {e}");
                        push_stream_chunk_to_state(&state, stream_id, Vec::new(), true);
                        break;
                    }
                    None => {
                        // Normal EOF
                        push_stream_chunk_to_state(&state, stream_id, Vec::new(), true);
                        break;
                    }
                };

                total = total.saturating_add(chunk.len());
                if total > MAX_RESPONSE_SIZE {
                    eprintln!(
                        "[fetch] response on stream {stream_id} exceeded MAX_RESPONSE_SIZE ({MAX_RESPONSE_SIZE}); truncating"
                    );
                    push_stream_chunk_to_state(&state, stream_id, Vec::new(), true);
                    break;
                }

                if chunk.is_empty() {
                    continue;
                }
                push_stream_chunk_to_state(&state, stream_id, chunk.to_vec(), false);
            }

            release_fetch_slot(&state);
        }).await;
    })
    .detach();
}

/// Deliver a body chunk from the detached fetch reader into the stream
/// backing `stream_id`.
///
/// ## Fast path (no V8 entry)
///
/// If no JS reader is currently blocked on `stream.read()` (i.e.
/// `pending_read` is `None`), the chunk goes straight into
/// `StreamState.buffer` under a short `RefCell` borrow and the pump
/// doesn't need to run at all. The next time JS calls `.read()`, the
/// chunk is already waiting. For a 1000-token LLM stream being consumed
/// at about the same rate it's produced, this path handles ~every chunk
/// — saving one `Box::pin`, one mpsc send, one V8 enter/exit, and one
/// microtask checkpoint per chunk. That was the 16% `fetch → echo`
/// regression flagged in the earlier perf review.
///
/// ## Slow path (V8 resolve)
///
/// If JS IS currently awaiting (or if the stream is being closed and we
/// need to signal done), we fall back to enqueuing an
/// `OpResult::StreamChunk` so the pump can enter V8, resolve the
/// `pending_read` resolver, and run microtasks. This is the path the
/// original implementation always used.
fn push_stream_chunk_to_state(
    state: &SharedState,
    stream_id: u32,
    data: Vec<u8>,
    done: bool,
) {
    // Fast path attempt under a single short borrow.
    // Three-way outcome: handled inline, needs-V8-resolve, or stream gone.
    enum Dispatch {
        /// Chunk was buffered; no V8 work needed. Most common case.
        Buffered,
        /// JS has a pending read or the stream needs to be closed — we
        /// must enter V8 to resolve a promise or run `close()` properly.
        NeedsV8,
        /// Stream no longer in state (e.g. JS cancelled it). Drop the
        /// chunk silently, but preserve the `done` signal in case the
        /// StreamState was never created yet (lazy-create in push_stream_chunk).
        DropIfExists,
    }

    // Classify into fast/slow path while holding a short state borrow.
    // On the fast path we also consume `data` by moving it into the
    // buffer; on the slow path we return `data` so the caller can hand
    // it to the OpResult future. `Option` avoids a clone for the common
    // fast-path buffering case.
    let (dispatch, returned_data) = {
        let mut s = state.borrow_mut();
        match s.streams.get_mut(&stream_id) {
            Some(stream) => {
                if done || stream.pending_read.is_some() {
                    (Dispatch::NeedsV8, Some(data))
                } else {
                    if !data.is_empty() {
                        stream.buffer.push_back(data);
                    }
                    (Dispatch::Buffered, None)
                }
            }
            None => {
                // push_stream_chunk's slow path lazy-creates StreamState
                // when needed (e.g. if JS hasn't constructed the stream
                // yet). We need the slow path for the close signal, but
                // we can drop non-done empty chunks into the void.
                if done || !data.is_empty() {
                    (Dispatch::NeedsV8, Some(data))
                } else {
                    (Dispatch::DropIfExists, None)
                }
            }
        }
    };

    match dispatch {
        Dispatch::Buffered | Dispatch::DropIfExists => {}
        Dispatch::NeedsV8 => {
            let chunk_data = returned_data.unwrap_or_default();
            let future: Pin<Box<dyn Future<Output = OpResult>>> = Box::pin(async move {
                OpResult::StreamChunk { stream_id, data: chunk_data, done }
            });

            let notify = {
                let mut s = state.borrow_mut();
                s.spawned_ops.push(future);
                s.pump_notify_tx.clone()
            };

            if let Some(tx) = notify {
                let _ = tx.clone().try_send(());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Legacy helpers kept only for tests — the production path now streams.
// ---------------------------------------------------------------------------

#[cfg(test)]
async fn buffer_response(
    response: cyper::Response,
    original_url: &str,
    cancel: Option<&crate::channel::CancelFlag>,
) -> Result<String, String> {
    // If the owning request was cancelled while headers were in flight,
    // drop the response without reading the body.
    if let Some(flag) = cancel
        && flag.is_cancelled()
    {
        return Err(error_json("fetch aborted: request cancelled"));
    }

    let status = response.status().as_u16();
    let status_text = response.status().canonical_reason().unwrap_or("").to_string();
    let final_url = response.url().to_string();
    let redirected = final_url != original_url;

    let mut resp_headers: Vec<(String, String)> = Vec::new();
    for (key, value) in response.headers() {
        if let Ok(v) = value.to_str() {
            resp_headers.push((key.to_string(), v.to_string()));
        }
    }

    if let Some(len) = response.content_length()
        && len > MAX_RESPONSE_SIZE as u64
    {
        return Err(error_json(&format!(
            "Response too large: {len} bytes (max {MAX_RESPONSE_SIZE})"
        )));
    }

    let body_bytes = response.bytes().await
        .map_err(|e| error_json(&format!("Failed to read response body: {e}")))?;

    if body_bytes.len() > MAX_RESPONSE_SIZE {
        return Err(error_json(&format!(
            "Response too large: {} bytes",
            body_bytes.len()
        )));
    }

    let body_text = String::from_utf8_lossy(&body_bytes).to_string();

    Ok(serde_json::json!({
        "status": status,
        "statusText": status_text,
        "headers": resp_headers,
        "body": body_text,
        "url": final_url,
        "redirected": redirected,
    })
    .to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn blocks_loopback_v4() {
        assert!(is_blocked_ip(Ipv4Addr::new(127, 0, 0, 1).into()));
    }

    #[test]
    fn blocks_private_v4() {
        assert!(is_blocked_ip(Ipv4Addr::new(10, 0, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(172, 20, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(192, 168, 1, 1).into()));
    }

    #[test]
    fn blocks_link_local_v4() {
        assert!(is_blocked_ip(Ipv4Addr::new(169, 254, 169, 254).into()));
    }

    #[test]
    fn blocks_cgnat_v4() {
        // AWS uses 100.64/10 for VPC ENIs — must be blocked
        assert!(is_blocked_ip(Ipv4Addr::new(100, 64, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(100, 127, 255, 254).into()));
    }

    #[test]
    fn blocks_multicast_v4() {
        assert!(is_blocked_ip(Ipv4Addr::new(224, 0, 0, 1).into()));
    }

    #[test]
    fn blocks_reserved_v4() {
        assert!(is_blocked_ip(Ipv4Addr::new(240, 0, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(255, 255, 255, 255).into()));
    }

    #[test]
    fn blocks_v4_mapped_v6() {
        // ::ffff:127.0.0.1 — v4-mapped form must be blocked
        let mapped: Ipv6Addr = "::ffff:7f00:1".parse().unwrap();
        assert!(is_blocked_ip(mapped.into()));
    }

    #[test]
    fn blocks_unique_local_v6() {
        assert!(is_blocked_ip("fc00::1".parse::<Ipv6Addr>().unwrap().into()));
        assert!(is_blocked_ip("fd00::1".parse::<Ipv6Addr>().unwrap().into()));
    }

    #[test]
    fn blocks_link_local_v6() {
        assert!(is_blocked_ip("fe80::1".parse::<Ipv6Addr>().unwrap().into()));
    }

    #[test]
    fn allows_public_v4() {
        assert!(!is_blocked_ip(Ipv4Addr::new(1, 1, 1, 1).into()));
        assert!(!is_blocked_ip(Ipv4Addr::new(8, 8, 8, 8).into()));
    }

    #[test]
    fn allows_public_v6() {
        assert!(!is_blocked_ip(
            "2606:4700:4700::1111".parse::<Ipv6Addr>().unwrap().into()
        ));
    }

    #[test]
    fn validate_url_rejects_localhost() {
        assert!(validate_url("http://localhost/x").is_err());
    }

    #[test]
    fn validate_url_rejects_literal_private_ip() {
        assert!(validate_url("http://10.0.0.1/x").is_err());
        assert!(validate_url("http://169.254.169.254/latest/meta-data").is_err());
    }

    #[test]
    fn validate_url_rejects_non_http() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("gopher://x/").is_err());
    }

    #[test]
    fn validate_url_allows_public_http() {
        assert!(validate_url("https://example.com/x").is_ok());
    }
}
