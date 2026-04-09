//! Reqwest-based fetch execution for the compio runtime.
//!
//! Since compio doesn't have a native HTTP client yet, fetch requests are
//! dispatched to a background tokio thread running reqwest.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use appbase_v8_core::fetch::{error_json, validate_url, parse_headers, MAX_RESPONSE_SIZE};
use appbase_v8_core::state::{FetchRequest, OpResult};

/// Shared reqwest Client -- reuses TCP connections and TLS sessions.
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

/// Background tokio runtime for running reqwest fetch requests.
/// Lazily initialized on first use.
fn fetch_tokio_handle() -> &'static tokio::runtime::Handle {
    use std::sync::OnceLock;
    static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("Failed to create fetch tokio runtime");
        let handle = rt.handle().clone();
        // Leak the runtime so it lives forever
        std::mem::forget(rt);
        handle
    })
}

/// Execute a `FetchRequest` by spawning on the background tokio runtime.
/// Returns a future that can be polled from the compio event loop.
pub(crate) fn execute_fetch(
    req: FetchRequest,
) -> Pin<Box<dyn Future<Output = OpResult>>> {
    let FetchRequest { op_id, stream_id: _, request_id, method, url, headers_json, body, cancel } = req;

    let handle = fetch_tokio_handle();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel::<String>();

    handle.spawn(async move {
        let value = match build_and_send_request(&method, &url, &headers_json, body.as_deref(), cancel.as_ref()).await {
            Ok(response) => buffer_response(response, &url).await,
            Err(err_json) => err_json,
        };
        let _ = result_tx.send(value);
    });

    Box::pin(async move {
        match result_rx.await {
            Ok(value) => OpResult::Completed { op_id, value, request_id },
            Err(_) => OpResult::Cancelled,
        }
    })
}

// ---------------------------------------------------------------------------
// Shared request building
// ---------------------------------------------------------------------------

async fn build_and_send_request(
    method: &str,
    url: &str,
    headers_json: &str,
    body: Option<&str>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<reqwest::Response, String> {
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

    if let Some(body) = body {
        request = request.body(body.to_string());
    }

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
// Buffered response
// ---------------------------------------------------------------------------

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
