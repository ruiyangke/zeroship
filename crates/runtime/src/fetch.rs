//! Native `__rawFetch` V8 callback — spawns HTTP requests via reqwest.
//!
//! Called from JS as: `__rawFetch(method, url, headersJson, body)` -> Promise<string>
//!
//! The fetch future is pushed into `state.spawned_ops`. The main `select!` loop
//! in runtime.rs polls it and resolves the promise when the response arrives.
//!
//! Small responses (≤1 MB or known content-length ≤1 MB) are fully buffered.
//! Large/unknown-size responses stream: headers are sent immediately via
//! `OpResult::Completed` (with `__stream: true`), then body chunks follow as
//! `OpResult::StreamChunk` events through `stream_events_tx`.

use std::net::IpAddr;
use std::time::Duration;

use crate::state::{OpResult, SharedState};
use tokio_util::sync::CancellationToken;

/// Maximum response body size: 10 MB.
const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024;

/// Build an error JSON string using serde_json.
fn error_json(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

/// Validate the URL to prevent SSRF attacks.
///
/// Blocks private/internal IPs, loopback, link-local, and non-HTTP(S) schemes.
fn validate_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL: {e}"))?;

    // Only allow http and https schemes
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => return Err(format!("Blocked URL scheme: {scheme}")),
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

    if let Some(addr) = ip {
        match addr {
            IpAddr::V4(v4) => {
                if v4.is_loopback()           // 127.0.0.0/8
                    || v4.is_private()         // 10/8, 172.16/12, 192.168/16
                    || v4.is_link_local()      // 169.254/16
                    || v4.is_unspecified()     // 0.0.0.0
                    || v4.is_broadcast()       // 255.255.255.255
                {
                    return Err(format!("Blocked request to private/internal IP: {v4}"));
                }
            }
            IpAddr::V6(v6) => {
                if v6.is_loopback()        // ::1
                    || v6.is_unspecified()  // ::
                    || (v6.segments()[0] & 0xffc0) == 0xfe80  // fe80::/10 link-local
                {
                    return Err(format!("Blocked request to private/internal IPv6: {v6}"));
                }
            }
        }
    }

    Ok(())
}

/// Shared reqwest Client — reuses TCP connections and TLS sessions across fetch calls.
fn shared_client() -> &'static reqwest::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(20))
            .pool_max_idle_per_host(50)
            .build()
            .expect("Failed to create HTTP client")
    })
}

/// Streaming threshold: responses with unknown or >1 MB content-length stream.
const STREAM_THRESHOLD: u64 = 1024 * 1024;

/// Hand-written V8 callback for `__rawFetch(method, url, headersJson, body)`.
///
/// Creates a Promise, allocates an op-id, and pushes an async future into
/// `state.spawned_ops`. The event loop collects and polls these futures.
///
/// When `stream_events_tx` is available and the response is large (or has
/// unknown content-length), the fetch switches to streaming mode: headers are
/// sent immediately as `OpResult::Completed` (with `__stream: true`), and body
/// chunks follow as `OpResult::StreamChunk` events through the channel.
pub(crate) fn raw_fetch_callback(
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
    let global_resolver = v8::Global::new(scope, resolver);

    // Allocate op_id + stream_id, capture request context
    let (op_id, stream_id, request_id, cancel, stream_events_tx) = {
        let mut s = state.borrow_mut();
        let id = s.next_op_id;
        s.next_op_id += 1;
        s.pending_resolvers.insert(id, global_resolver);

        let sid = s.next_stream_id;
        s.next_stream_id += 1;

        let req_id = s.executing_request_id;
        let cancel = s.executing_request_cancel.clone();
        let stx = s.stream_events_tx.clone();

        (id, sid, req_id, cancel, stx)
    };

    // Capture server_handle before borrowing state mutably
    let server_handle = state.borrow().server_handle.clone();

    if let Some(handle) = server_handle {
        // Spawn fetch I/O on the server's multi-threaded tokio runtime.
        // Only a lightweight oneshot receiver runs on the local runtime.
        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<String>();
        handle.spawn(async move {
            // Build and send the HTTP request
            let response = match build_and_send_request(
                &method, &url, &headers_json, body.as_deref(), cancel.as_ref(),
            ).await {
                Ok(r) => r,
                Err(err_json) => {
                    let _ = result_tx.send(err_json);
                    return;
                }
            };

            // Decide: buffer or stream based on content-length
            let should_stream = stream_events_tx.is_some()
                && response.content_length().map_or(true, |len| len > STREAM_THRESHOLD);

            if should_stream {
                // Drop the oneshot — receiver will see Err → OpResult::Cancelled
                drop(result_tx);
                do_fetch_streaming_from_response(
                    response, op_id, stream_id, request_id,
                    stream_events_tx.unwrap(), cancel, &url,
                ).await;
            } else {
                // Buffer the entire body and send via oneshot
                let value = buffer_response(response, &url).await;
                let _ = result_tx.send(value);
            }
        });

        let receiver_future: std::pin::Pin<Box<dyn std::future::Future<Output = OpResult>>> =
            Box::pin(async move {
                match result_rx.await {
                    Ok(value) => OpResult::Completed { op_id, value, request_id },
                    Err(_) => OpResult::Cancelled,
                }
            });
        state.borrow_mut().spawned_ops.push(receiver_future);
    } else {
        // No server handle — run fetch on the local runtime (tests, standalone).
        let future: std::pin::Pin<Box<dyn std::future::Future<Output = OpResult>>> =
            Box::pin(async move {
                // Build and send the HTTP request
                let response = match build_and_send_request(
                    &method, &url, &headers_json, body.as_deref(), cancel.as_ref(),
                ).await {
                    Ok(r) => r,
                    Err(err_json) => {
                        return OpResult::Completed { op_id, value: err_json, request_id };
                    }
                };

                // Decide: buffer or stream based on content-length
                let should_stream = stream_events_tx.is_some()
                    && response.content_length().map_or(true, |len| len > STREAM_THRESHOLD);

                if should_stream {
                    do_fetch_streaming_from_response(
                        response, op_id, stream_id, request_id,
                        stream_events_tx.unwrap(), cancel, &url,
                    ).await;
                    // Real results already sent via stream_events_tx
                    OpResult::Cancelled
                } else {
                    let value = buffer_response(response, &url).await;
                    OpResult::Completed { op_id, value, request_id }
                }
            });

        state.borrow_mut().spawned_ops.push(future);
    }

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Shared request building
// ---------------------------------------------------------------------------

/// Build and send an HTTP request, returning the `reqwest::Response`.
///
/// Validates the URL (SSRF protection), parses method/headers/body, sends the
/// request. If a `CancellationToken` is provided, the send is raced against it.
/// Returns `Err(json)` with an error JSON string on failure.
async fn build_and_send_request(
    method: &str,
    url: &str,
    headers_json: &str,
    body: Option<&str>,
    cancel: Option<&CancellationToken>,
) -> Result<reqwest::Response, String> {
    // SSRF protection: validate URL before making any request
    if let Err(msg) = validate_url(url) {
        return Err(error_json(&msg));
    }

    let client = shared_client();

    let reqwest_method = match method.to_uppercase().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        "OPTIONS" => reqwest::Method::OPTIONS,
        other => match reqwest::Method::from_bytes(other.as_bytes()) {
            Ok(m) => m,
            Err(e) => return Err(error_json(&format!("Invalid HTTP method: {e}"))),
        },
    };

    let mut request = client.request(reqwest_method, url);

    // Parse headers
    if !headers_json.is_empty() {
        match parse_headers(headers_json) {
            Ok(headers) => {
                for (key, value) in headers {
                    request = request.header(&key, &value);
                }
            }
            Err(e) => return Err(error_json(&format!("Invalid headers: {e}"))),
        }
    }

    // Set body
    if let Some(body) = body {
        request = request.body(body.to_string());
    }

    // Send request, optionally racing against cancellation
    let send_fut = request.send();
    let response = if let Some(token) = cancel {
        tokio::select! {
            result = send_fut => result.map_err(|e| error_json(&e.to_string()))?,
            _ = token.cancelled() => return Err(error_json("request cancelled")),
        }
    } else {
        send_fut.await.map_err(|e| error_json(&e.to_string()))?
    };

    Ok(response)
}

// ---------------------------------------------------------------------------
// Buffered response path
// ---------------------------------------------------------------------------

/// Read the entire response body and return a JSON string with status, headers,
/// body, url, and redirected fields.
async fn buffer_response(response: reqwest::Response, original_url: &str) -> String {
    match buffer_response_inner(response, original_url).await {
        Ok(json) => json,
        Err(err_json) => err_json,
    }
}

async fn buffer_response_inner(
    response: reqwest::Response,
    original_url: &str,
) -> Result<String, String> {
    let status = response.status().as_u16();
    let status_text = response.status().canonical_reason().unwrap_or("").to_string();
    let final_url = response.url().to_string();
    let redirected = final_url != original_url;

    // Collect response headers
    let mut resp_headers: Vec<(String, String)> = Vec::new();
    for (key, value) in response.headers() {
        if let Ok(v) = value.to_str() {
            resp_headers.push((key.to_string(), v.to_string()));
        }
    }

    // Check content-length hint before reading body
    if let Some(len) = response.content_length() {
        if len > MAX_RESPONSE_SIZE as u64 {
            return Err(error_json(&format!(
                "Response too large: {} bytes (max {})",
                len, MAX_RESPONSE_SIZE
            )));
        }
    }

    // Read full body (buffered)
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => return Err(error_json(&format!("Failed to read response body: {e}"))),
    };

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
// Streaming response path
// ---------------------------------------------------------------------------

/// Stream a response: send headers immediately via `stream_events_tx`, then
/// send body chunks as `OpResult::StreamChunk` events.
///
/// On error, sends the error as `OpResult::Completed` so the promise rejects.
async fn do_fetch_streaming_from_response(
    response: reqwest::Response,
    op_id: u32,
    stream_id: u32,
    request_id: Option<u64>,
    stream_events_tx: tokio::sync::mpsc::Sender<OpResult>,
    cancel: Option<CancellationToken>,
    original_url: &str,
) {
    match do_fetch_streaming_inner(
        response, op_id, stream_id, request_id, &stream_events_tx, cancel.as_ref(), original_url,
    ).await {
        Ok(()) => {}
        Err(err_json) => {
            // Send error as OpResult::Completed so the promise rejects
            let _ = stream_events_tx.send(OpResult::Completed {
                op_id,
                value: err_json,
                request_id,
            }).await;
        }
    }
}

async fn do_fetch_streaming_inner(
    mut response: reqwest::Response,
    op_id: u32,
    stream_id: u32,
    request_id: Option<u64>,
    stream_events_tx: &tokio::sync::mpsc::Sender<OpResult>,
    cancel: Option<&CancellationToken>,
    original_url: &str,
) -> Result<(), String> {
    let status = response.status().as_u16();
    let status_text = response.status().canonical_reason().unwrap_or("").to_string();
    let final_url = response.url().to_string();
    let redirected = final_url != original_url;

    // Collect response headers
    let mut resp_headers: Vec<(String, String)> = Vec::new();
    for (key, value) in response.headers() {
        if let Ok(v) = value.to_str() {
            resp_headers.push((key.to_string(), v.to_string()));
        }
    }

    // Send headers immediately (with stream marker)
    let headers_json = serde_json::json!({
        "status": status,
        "statusText": status_text,
        "headers": resp_headers,
        "url": final_url,
        "redirected": redirected,
        "__stream": true,
        "stream_id": stream_id,
    })
    .to_string();

    stream_events_tx
        .send(OpResult::Completed {
            op_id,
            value: headers_json,
            request_id,
        })
        .await
        .map_err(|_| error_json("stream_events channel closed"))?;

    // Stream body chunks
    loop {
        let chunk_result = if let Some(token) = cancel {
            tokio::select! {
                chunk = response.chunk() => chunk,
                _ = token.cancelled() => {
                    // Send a final done chunk so the stream closes cleanly
                    let _ = stream_events_tx.send(OpResult::StreamChunk {
                        stream_id,
                        data: Vec::new(),
                        done: true,
                    }).await;
                    return Err(error_json("request cancelled"));
                }
            }
        } else {
            response.chunk().await
        };

        match chunk_result {
            Ok(Some(chunk)) => {
                stream_events_tx
                    .send(OpResult::StreamChunk {
                        stream_id,
                        data: chunk.to_vec(),
                        done: false,
                    })
                    .await
                    .map_err(|_| error_json("stream_events channel closed"))?;
            }
            Ok(None) => {
                // End of stream
                let _ = stream_events_tx
                    .send(OpResult::StreamChunk {
                        stream_id,
                        data: Vec::new(),
                        done: true,
                    })
                    .await;
                break;
            }
            Err(e) => {
                // Send done chunk so JS side sees EOF, then return error
                let _ = stream_events_tx
                    .send(OpResult::StreamChunk {
                        stream_id,
                        data: Vec::new(),
                        done: true,
                    })
                    .await;
                return Err(error_json(&format!("Failed to read response chunk: {e}")));
            }
        }
    }

    Ok(())
}

/// Parse headers from JSON — supports both `[["key","val"],...]` and `{"key":"val",...}` formats.
fn parse_headers(json: &str) -> Result<Vec<(String, String)>, String> {
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
