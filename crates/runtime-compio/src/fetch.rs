//! cyper-based fetch execution for the compio runtime.
//!
//! cyper is a native compio HTTP client — requests run directly on the
//! io_uring event loop with no background tokio thread.

use std::future::Future;
use std::pin::Pin;

use appbase_v8_core::fetch::{error_json, validate_url, parse_headers, MAX_RESPONSE_SIZE};
use appbase_v8_core::state::{FetchRequest, OpResult};

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
