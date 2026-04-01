//! Native `__rawFetch` V8 callback — spawns HTTP requests via reqwest.
//!
//! Called from JS as: `__rawFetch(method, url, headersJson, body)` -> Promise<string>
//! The resolved string is JSON: `{status, statusText, headers, body, url, redirected}` or `{error}`.

use std::time::Duration;

use crate::event_loop::{OpResult, SharedState};

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
            Err(e) => return format!(r#"{{"error":"Invalid HTTP method: {}"}}"#, escape_json(&e.to_string())),
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
            Err(e) => return format!(r#"{{"error":"Invalid headers: {}"}}"#, escape_json(&e)),
        }
    }

    // Set body
    if let Some(body) = body {
        request = request.body(body.to_string());
    }

    // Send request
    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => return format!(r#"{{"error":"{}"}}"#, escape_json(&e.to_string())),
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

    // Read body
    let body_text = match response.text().await {
        Ok(t) => t,
        Err(e) => return format!(r#"{{"error":"Failed to read response body: {}"}}"#, escape_json(&e.to_string())),
    };

    // Build response JSON
    let headers_json = serde_json::to_string(&resp_headers).unwrap_or_else(|_| "[]".to_string());

    format!(
        r#"{{"status":{},"statusText":"{}","headers":{},"body":"{}","url":"{}","redirected":{}}}"#,
        status,
        escape_json(&status_text),
        headers_json,
        escape_json(&body_text),
        escape_json(&final_url),
        redirected,
    )
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

/// Escape a string for inclusion in a JSON string value.
fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}
