//! Reqwest-based fetch execution — drains `FetchRequest`s from v8-core state
//! and executes them as async HTTP requests.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use appbase_v8_core::fetch::{error_json, validate_url, parse_headers, MAX_RESPONSE_SIZE};
use appbase_v8_core::state::{FetchRequest, OpResult};
use tokio_util::sync::CancellationToken;

/// Streaming threshold: responses with unknown or >1 MB content-length stream.
const STREAM_THRESHOLD: u64 = 1024 * 1024;

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

/// Execute a `FetchRequest` and return the future that produces an `OpResult`.
///
/// If a `server_handle` is present, I/O is spawned on the multi-threaded runtime
/// and results are delivered via a oneshot channel. Otherwise, the fetch runs
/// on the local (current-thread) runtime.
pub(crate) fn execute_fetch(
    req: FetchRequest,
    server_handle: Option<&tokio::runtime::Handle>,
    stream_events_tx: Option<tokio::sync::mpsc::Sender<OpResult>>,
) -> Pin<Box<dyn Future<Output = OpResult>>> {
    let FetchRequest { op_id, stream_id, request_id, method, url, headers_json, body, cancel } = req;

    if let Some(handle) = server_handle {
        let stx = stream_events_tx;

        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<String>();
        handle.spawn(async move {
            let response = match build_and_send_request(
                &method, &url, &headers_json, body.as_deref(), cancel.as_ref(),
            ).await {
                Ok(r) => r,
                Err(err_json) => {
                    let _ = result_tx.send(err_json);
                    return;
                }
            };

            let should_stream = stx.is_some()
                && response.content_length().map_or(true, |len| len > STREAM_THRESHOLD);

            if should_stream {
                drop(result_tx);
                do_fetch_streaming_from_response(
                    response, op_id, stream_id, request_id,
                    stx.unwrap(), cancel, &url,
                ).await;
            } else {
                let value = buffer_response(response, &url).await;
                let _ = result_tx.send(value);
            }
        });

        Box::pin(async move {
            match result_rx.await {
                Ok(value) => OpResult::Completed { op_id, value, request_id },
                Err(_) => OpResult::Cancelled,
            }
        })
    } else {
        let stx = stream_events_tx;

        Box::pin(async move {
            let response = match build_and_send_request(
                &method, &url, &headers_json, body.as_deref(), cancel.as_ref(),
            ).await {
                Ok(r) => r,
                Err(err_json) => {
                    return OpResult::Completed { op_id, value: err_json, request_id };
                }
            };

            let should_stream = stx.is_some()
                && response.content_length().map_or(true, |len| len > STREAM_THRESHOLD);

            if should_stream {
                do_fetch_streaming_from_response(
                    response, op_id, stream_id, request_id,
                    stx.unwrap(), cancel, &url,
                ).await;
                OpResult::Cancelled
            } else {
                let value = buffer_response(response, &url).await;
                OpResult::Completed { op_id, value, request_id }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Shared request building
// ---------------------------------------------------------------------------

async fn build_and_send_request(
    method: &str,
    url: &str,
    headers_json: &str,
    body: Option<&str>,
    cancel: Option<&CancellationToken>,
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
// Buffered response path
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
                "Response too large: {} bytes (max {})",
                len, MAX_RESPONSE_SIZE
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

// ---------------------------------------------------------------------------
// Streaming response path
// ---------------------------------------------------------------------------

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

    let mut resp_headers: Vec<(String, String)> = Vec::new();
    for (key, value) in response.headers() {
        if let Ok(v) = value.to_str() {
            resp_headers.push((key.to_string(), v.to_string()));
        }
    }

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

    loop {
        let chunk_result = if let Some(token) = cancel {
            tokio::select! {
                chunk = response.chunk() => chunk,
                _ = token.cancelled() => {
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
