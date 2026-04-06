//! Native `__rawFetch` V8 callback — spawns HTTP requests via reqwest.
//!
//! Called from JS as: `__rawFetch(method, url, headersJson, body)` -> Promise<string>
//!
//! **Per-request model (streaming):** The promise resolves when HEADERS arrive.
//! The resolved JSON includes `stream_id` instead of `body`. Body chunks are
//! delivered as `LoopEvent::StreamChunk` through the event channel.
//!
//! **Concurrent model (full-body):** Uses the old approach — reads the full body
//! before sending `Event::OpCompleted`. No streaming, no `stream_id`.

use std::net::IpAddr;
use std::time::Duration;

use crate::event_loop::{LoopEvent, SharedState};

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

/// Hand-written V8 callback for `__rawFetch(method, url, headersJson, body)`.
///
/// Creates a Promise, allocates a stream_id, spawns a tokio task that:
/// 1. Sends the request
/// 2. On headers: sends `LoopEvent::OpCompleted` with `{status, headers, stream_id}`
/// 3. On each body chunk: sends `LoopEvent::StreamChunk`
/// 4. On completion: sends `LoopEvent::StreamChunk { done: true }`
///
/// For the concurrent model (`concurrent_event_tx` is Some), falls back to the
/// full-body approach — reads entire body, sends `Event::OpCompleted`.
pub(crate) fn raw_fetch_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopInner not in isolate slot")
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

    // Allocate op_id and grab channel senders.
    // stream_id is allocated but StreamState is NOT created until streaming is needed
    // (avoids HashMap insert + allocation for buffered responses).
    let (op_id, stream_id, event_tx, waker, concurrent_tx, tokio_handle) = {
        let mut s = state.borrow_mut();
        let id = s.next_op_id;
        s.next_op_id += 1;
        s.pending_resolvers.insert(id, global_resolver);

        let sid = s.next_stream_id;
        s.next_stream_id += 1;
        // NOTE: StreamState is created lazily in do_fetch_streaming only when
        // the streaming path is taken. For buffered responses, no StreamState exists.

        (
            id,
            sid,
            s.event_tx.clone(),
            s.waker.clone(),
            s.concurrent_event_tx.clone(),
            s.tokio_handle.clone(),
        )
    };

    let task = async move {
        let result = do_fetch_streaming(
            &method, &url, &headers_json, body.as_deref(),
            op_id, stream_id, &event_tx, &waker, concurrent_tx.as_ref(),
        ).await;

        // If do_fetch_streaming returned an error string, send it as OpCompleted
        // (the error JSON is already formatted for JS consumption)
        if let Err(err_json) = result {
            match concurrent_tx {
                Some(ref ctx) => {
                    let _ = ctx.send(crate::concurrent::Event::OpCompleted {
                        op_id,
                        value: err_json,
                    });
                }
                None => {
                    let _ = event_tx.send(LoopEvent::OpCompleted {
                        id: op_id,
                        value: err_json,
                    });
                    waker.wake();
                }
            }
        }
    };

    match tokio_handle {
        Some(handle) => {
            handle.spawn(task);
        }
        None => {
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create tokio runtime for async op");
                rt.block_on(task);
            });
        }
    }

    rv.set(promise.into());
}

/// Perform HTTP fetch with streaming body delivery.
///
/// Returns `Ok(())` on success (events already sent), or `Err(error_json)` if
/// the request failed before headers could be sent.
async fn do_fetch_streaming(
    method: &str,
    url: &str,
    headers_json: &str,
    body: Option<&str>,
    op_id: u32,
    stream_id: u32,
    event_tx: &std::sync::mpsc::Sender<LoopEvent>,
    waker: &futures::task::AtomicWaker,
    concurrent_tx: Option<&crate::concurrent::EventSender>,
) -> Result<(), String> {
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

    // Send request
    let mut response = match request.send().await {
        Ok(r) => r,
        Err(e) => return Err(error_json(&e.to_string())),
    };

    let status = response.status().as_u16();
    let status_text = response.status().canonical_reason().unwrap_or("").to_string();
    let final_url = response.url().to_string();
    let redirected = final_url != url;

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

    // Decide: buffer full body (fast for small responses) or stream chunks (needed for large/SSE).
    // Threshold: if content-length is known and small, or it's the concurrent model, buffer it.
    let should_stream = concurrent_tx.is_none()
        && response.content_length().map_or(true, |len| len > 1024 * 1024); // >1MB → stream

    if !should_stream {
        // ---------------------------------------------------------------
        // Buffered path — full body in one OpCompleted (fast for small responses)
        // ---------------------------------------------------------------
        let body_bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => return Err(error_json(&format!("Failed to read response body: {e}"))),
        };

        if body_bytes.len() > MAX_RESPONSE_SIZE {
            return Err(error_json(&format!("Response too large: {} bytes", body_bytes.len())));
        }

        let body_text = String::from_utf8_lossy(&body_bytes).to_string();

        let result = serde_json::json!({
            "status": status,
            "statusText": status_text,
            "headers": resp_headers,
            "body": body_text,
            "url": final_url,
            "redirected": redirected,
        })
        .to_string();

        if let Some(ctx) = concurrent_tx {
            let _ = ctx.send(crate::concurrent::Event::OpCompleted {
                op_id,
                value: result,
            });
        } else {
            let _ = event_tx.send(LoopEvent::OpCompleted {
                id: op_id,
                value: result,
            });
            waker.wake();
        }
    } else {
        // ---------------------------------------------------------------
        // Streaming path — headers first, body chunks via StreamChunk
        // (for large responses >1MB, SSE, chunked transfer, unknown length)
        // ---------------------------------------------------------------

        // Send headers + stream_id as OpCompleted (resolves the JS Promise)
        let header_result = serde_json::json!({
            "status": status,
            "statusText": status_text,
            "headers": resp_headers,
            "url": final_url,
            "redirected": redirected,
            "stream_id": stream_id,
        })
        .to_string();

        let _ = event_tx.send(LoopEvent::OpCompleted {
            id: op_id,
            value: header_result,
        });
        waker.wake();

        // Stream body chunks
        let mut total_bytes: usize = 0;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    total_bytes += chunk.len();
                    if total_bytes > MAX_RESPONSE_SIZE {
                        let _ = event_tx.send(LoopEvent::StreamChunk {
                            stream_id,
                            data: format!("Error: Response too large: {total_bytes} bytes").into_bytes(),
                            done: true,
                        });
                        waker.wake();
                        return Ok(());
                    }
                    let _ = event_tx.send(LoopEvent::StreamChunk {
                        stream_id,
                        data: chunk.to_vec(),
                        done: false,
                    });
                    waker.wake();
                }
                Ok(None) => {
                    // Body complete
                    let _ = event_tx.send(LoopEvent::StreamChunk {
                        stream_id,
                        data: vec![],
                        done: true,
                    });
                    waker.wake();
                    break;
                }
                Err(e) => {
                    let _ = event_tx.send(LoopEvent::StreamChunk {
                        stream_id,
                        data: format!("Error: {e}").into_bytes(),
                        done: true,
                    });
                    waker.wake();
                    return Ok(());
                }
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
