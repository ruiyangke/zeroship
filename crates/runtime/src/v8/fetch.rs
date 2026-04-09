//! Native `__rawFetch` V8 callback — queues fetch requests for the runtime executor.
//!
//! Called from JS as: `__rawFetch(method, url, headersJson, body)` -> Promise<string>
//!
//! The fetch request is pushed into `state.spawned_fetches`. The runtime executor
//! (e.g. runtime-tokio) drains these and spawns the actual HTTP I/O.

use std::net::IpAddr;

use crate::v8::state::{FetchRequest, SharedState};

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
