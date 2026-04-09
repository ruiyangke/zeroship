//! Complete fetch lifecycle — V8 callback + cyper HTTP execution.
//!
//! 1. `raw_fetch_callback`: V8 callback, pushes FetchRequest into state
//! 2. `execute_fetch`: cyper HTTP client execution (called by runtime.rs pump)
//! 3. Validation: `validate_url` (SSRF), `parse_headers`

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;

use crate::state::{FetchRequest, OpResult, SharedState};

/// Maximum response body size: 10 MB.
pub const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024;

/// Build an error JSON string using serde_json.
pub fn error_json(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

/// Validate the URL to prevent SSRF attacks.
///
/// Blocks private/internal IPs, loopback, link-local, and non-HTTP(S) schemes.
pub fn validate_url(url: &str) -> Result<(), String> {
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
    let global_resolver = v8::Global::new(scope, resolver);

    // Allocate op_id + stream_id, capture request context
    let (op_id, stream_id, request_id, cancel) = {
        let mut s = state.borrow_mut();
        let id = s.next_op_id;
        s.next_op_id += 1;
        s.pending_resolvers.insert(id, global_resolver);

        let sid = s.next_stream_id;
        s.next_stream_id += 1;

        let req_id = s.executing_request_id;
        let cancel = s.executing_request_cancel.clone();

        (id, sid, req_id, cancel)
    };

    // Queue the fetch request for the runtime executor
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
fn shared_client() -> &'static cyper::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<cyper::Client> = OnceLock::new();
    CLIENT.get_or_init(cyper::Client::new)
}

/// Execute a `FetchRequest` on the compio event loop via cyper.
/// Returns a future that resolves to an `OpResult`.
pub(crate) fn execute_fetch(
    req: FetchRequest,
) -> Pin<Box<dyn Future<Output = OpResult>>> {
    let FetchRequest { op_id, stream_id: _, request_id, method, url, headers_json, body, cancel: _ } = req;

    Box::pin(async move {
        let value = match build_and_send_request(&method, &url, &headers_json, body.as_deref()).await {
            Ok(json) => json,
            Err(err_json) => err_json,
        };
        OpResult::Completed { op_id, value, request_id }
    })
}

// ---------------------------------------------------------------------------
// Request building & sending
// ---------------------------------------------------------------------------

async fn build_and_send_request(
    method: &str,
    url: &str,
    headers_json: &str,
    body: Option<&str>,
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

    let response = builder.send().await
        .map_err(|e| error_json(&e.to_string()))?;

    buffer_response(response, url).await
}

// ---------------------------------------------------------------------------
// Buffered response
// ---------------------------------------------------------------------------

async fn buffer_response(response: cyper::Response, original_url: &str) -> Result<String, String> {
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

    if let Some(len) = response.content_length() {
        if len > MAX_RESPONSE_SIZE as u64 {
            return Err(error_json(&format!(
                "Response too large: {len} bytes (max {MAX_RESPONSE_SIZE})"
            )));
        }
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
