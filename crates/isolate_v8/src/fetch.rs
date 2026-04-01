//! Native `__rawFetch` V8 callback — spawns HTTP requests via reqwest.
//!
//! Called from JS as: `__rawFetch(method, url, headersJson, body)` -> Promise<string>
//! The resolved string is JSON: `{status, statusText, headers, body, url, redirected}` or `{error}`.

use std::net::IpAddr;
use std::time::Duration;

use crate::event_loop::{OpResult, SharedState};

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

/// V8 callback for `__rawFetch(method, url, headersJson, body)`.
/// Returns a Promise that resolves with a JSON string.
pub(crate) fn raw_fetch_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();

    // Extract arguments
    let method = if args.length() > 0 {
        args.get(0).to_rust_string_lossy(scope)
    } else {
        "GET".to_string()
    };

    let url = if args.length() > 1 {
        args.get(1).to_rust_string_lossy(scope)
    } else {
        let msg = v8::String::new(scope, "__rawFetch: url is required").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    };

    let headers_json = if args.length() > 2 && !args.get(2).is_null_or_undefined() {
        args.get(2).to_rust_string_lossy(scope)
    } else {
        String::new()
    };

    let body = if args.length() > 3 && !args.get(3).is_null_or_undefined() {
        Some(args.get(3).to_rust_string_lossy(scope))
    } else {
        None
    };

    // Create promise resolver
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let global_resolver = v8::Global::new(scope, resolver);

    // Assign op id and store resolver
    let (op_id, op_tx, concurrent_tx, tokio_handle) = {
        let mut s = state.borrow_mut();
        let id = s.next_op_id;
        s.next_op_id += 1;
        s.pending_resolvers.insert(id, global_resolver);
        (id, s.op_tx.clone(), s.concurrent_event_tx.clone(), s.tokio_handle.clone())
    };

    // Spawn the HTTP request
    match tokio_handle {
        Some(handle) => {
            handle.spawn(async move {
                let result = do_fetch(&method, &url, &headers_json, body.as_deref()).await;
                match concurrent_tx {
                    Some(tx) => {
                        let _ = tx.send(crate::concurrent::Event::OpCompleted { op_id, value: result });
                    }
                    None => {
                        let _ = op_tx.send(OpResult { id: op_id, value: result });
                    }
                }
            });
        }
        None => {
            // Fallback: spawn a std::thread with a one-shot tokio runtime
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create tokio runtime for fetch");
                let result = rt.block_on(do_fetch(&method, &url, &headers_json, body.as_deref()));
                match concurrent_tx {
                    Some(tx) => {
                        let _ = tx.send(crate::concurrent::Event::OpCompleted { op_id, value: result });
                    }
                    None => {
                        let _ = op_tx.send(OpResult { id: op_id, value: result });
                    }
                }
            });
        }
    }

    rv.set(promise.into());
}

/// Perform the actual HTTP fetch via reqwest. Returns a JSON string.
async fn do_fetch(method: &str, url: &str, headers_json: &str, body: Option<&str>) -> String {
    // SSRF protection: validate URL before making any request
    if let Err(msg) = validate_url(url) {
        return error_json(&msg);
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
            Err(e) => return error_json(&format!("Invalid HTTP method: {e}")),
        },
    };

    let mut request = client.request(reqwest_method, url);

    // Parse headers
    if !headers_json.is_empty() {
        let parsed_headers = parse_headers(headers_json);
        match parsed_headers {
            Ok(headers) => {
                for (key, value) in headers {
                    request = request.header(&key, &value);
                }
            }
            Err(e) => return error_json(&format!("Invalid headers: {e}")),
        }
    }

    // Set body
    if let Some(body) = body {
        request = request.body(body.to_string());
    }

    // Send request
    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => return error_json(&e.to_string()),
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
            return error_json(&format!(
                "Response too large: {} bytes (max {})",
                len, MAX_RESPONSE_SIZE
            ));
        }
    }

    // Read body with size limit
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => return error_json(&format!("Failed to read response body: {e}")),
    };

    if body_bytes.len() > MAX_RESPONSE_SIZE {
        return error_json(&format!("Response too large: {} bytes", body_bytes.len()));
    }

    let body_text = String::from_utf8_lossy(&body_bytes).to_string();

    // Build response JSON with serde_json
    serde_json::json!({
        "status": status,
        "statusText": status_text,
        "headers": resp_headers,
        "body": body_text,
        "url": final_url,
        "redirected": redirected,
    })
    .to_string()
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
