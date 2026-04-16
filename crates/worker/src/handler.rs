use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures::{pin_mut, FutureExt};
use ntex::web::{self, HttpRequest, HttpResponse};
use ntex::util::Bytes;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use zeroship_runtime::runtime::DispatchOutcome;
use zeroship_runtime::{ResultReceiver, RuntimeHandle, StreamReader};

use crate::{cache, WorkerConfig};

async fn recv_with_timeout<T>(
    rx: &ResultReceiver<T>,
    timeout: std::time::Duration,
) -> Option<T> {
    let recv = rx.recv().fuse();
    let sleep = compio::time::sleep(timeout).fuse();
    pin_mut!(recv, sleep);
    futures::select! {
        result = recv => Some(result),
        _ = sleep => None,
    }
}

fn wall_limit(handle: &RuntimeHandle) -> std::time::Duration {
    handle
        .wall_timeout()
        .unwrap_or(std::time::Duration::from_secs(30))
}

// ---------------------------------------------------------------------------
// HTTP dispatch — called by the gateway for apps with onRequest handlers
// ---------------------------------------------------------------------------

/// Dispatch an HTTP request through the V8 onRequest handler.
///
/// The gateway sends an HTTP envelope with the original request details
/// (method, URL, headers, body) so the JS handler receives a proper
/// `Request` object. The response may be buffered or streaming (SSE).
pub async fn http_dispatch(
    req: HttpRequest,
    config: web::types::State<Arc<WorkerConfig>>,
    path: web::types::Path<String>,
    body: Bytes,
) -> HttpResponse {
    let app_id = match path.parse::<Uuid>() {
        Ok(id) => id,
        Err(_) => return HttpResponse::BadRequest().body(r#"{"error":"invalid app_id"}"#),
    };

    // On-demand loading: if app is not cached, pull from control plane
    if cache::get_runtime(&app_id).is_none() {
        if let Err(e) = load_on_demand(&config, &app_id).await {
            return HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": format!("failed to load app: {e}")}));
        }
    }

    let handle = match cache::get_runtime(&app_id) {
        Some(handle) => handle,
        None => {
            return HttpResponse::NotFound()
                .body(format!(r#"{{"error":"app {app_id} not loaded"}}"#));
        }
    };
    let runtime = handle.runtime();

    // Decode authenticated user from ZeroShip-User header (base64 JSON from gateway)
    let user_json = req
        .headers()
        .get("zeroship-user")
        .and_then(|v| v.to_str().ok())
        .and_then(|b64| B64.decode(b64).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok());

    // Set auth user in thread-local before dispatch, clear after
    zeroship_runtime::auth::set_auth_user(user_json);

    // Parse the HTTP envelope from the request body
    let envelope: HttpEnvelope = match serde_json::from_slice(&body) {
        Ok(env) => env,
        Err(e) => {
            zeroship_runtime::auth::clear_auth_user();
            return HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": format!("invalid HTTP envelope: {e}")}));
        }
    };

    // Build headers JSON array for dispatch_http: [[key, value], ...]
    let headers_json = serde_json::to_string(&envelope.headers).unwrap_or_else(|_| "[]".into());

    // Phase 1: Enter isolate, start HTTP dispatch
    let outcome = {
        let mut rt = runtime.borrow_mut();
        rt.enter_isolate();
        let o = rt.dispatch_http(handle.modules(), &envelope.method, &envelope.url, &headers_json, &envelope.body);
        rt.exit_isolate();
        o
    };

    // Phase 2: Handle the outcome
    let response = match outcome {
        DispatchOutcome::HttpComplete { status, headers, body, logs: _ } => {
            make_http_response(status, &headers, &body)
        }
        DispatchOutcome::HttpStream { status, headers, body: reader, logs: _ } => {
            stream_response(status, &headers, reader)
        }
        DispatchOutcome::HttpPending(rx) => {
            match recv_with_timeout(&rx, wall_limit(&handle)).await {
                Some(Ok(zeroship_runtime::HttpDispatchResult::Complete { status, headers, body, .. })) => {
                    make_http_response(status, &headers, &body)
                }
                Some(Ok(zeroship_runtime::HttpDispatchResult::Stream { status, headers, body: reader, .. })) => {
                    stream_response(status, &headers, reader)
                }
                Some(Ok(zeroship_runtime::HttpDispatchResult::WebSocket { .. })) => {
                    make_error("WebSocket upgrade not supported via gateway dispatch")
                }
                Some(Err(e)) => make_error(&e),
                None => make_error("request timed out"),
            }
        }
        // dispatch_http never returns these, but handle exhaustively
        DispatchOutcome::Complete(Ok(result)) => {
            make_response(&result.json, result.cpu_time.as_secs_f64() * 1000.0)
        }
        DispatchOutcome::Complete(Err(e)) => make_error(&e),
        DispatchOutcome::Pending(_) => make_error("unexpected Pending from dispatch_http"),
        DispatchOutcome::WebSocketUpgrade { .. } => {
            make_error("WebSocket upgrade not supported via gateway dispatch")
        }
    };

    // Clear auth user after dispatch (prevent leaking to next request)
    zeroship_runtime::auth::clear_auth_user();

    response
}

/// JSON envelope for HTTP dispatch requests from the gateway.
#[derive(serde::Deserialize)]
struct HttpEnvelope {
    method: String,
    url: String,
    /// Headers as [[key, value], ...] array.
    headers: Vec<(String, String)>,
    #[serde(default)]
    body: String,
}

// ---------------------------------------------------------------------------
// RPC dispatch — existing path for JSON-RPC requests
// ---------------------------------------------------------------------------

pub async fn dispatch(
    req: HttpRequest,
    config: web::types::State<Arc<WorkerConfig>>,
    path: web::types::Path<String>,
    body: String,
) -> HttpResponse {
    let app_id = match path.parse::<Uuid>() {
        Ok(id) => id,
        Err(_) => return HttpResponse::BadRequest().body(r#"{"error":"invalid app_id"}"#),
    };

    // On-demand loading: if app is not cached, pull from control plane
    if cache::get_runtime(&app_id).is_none() {
        if let Err(e) = load_on_demand(&config, &app_id).await {
            return HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": format!("failed to load app: {e}")}));
        }
    }

    let handle = match cache::get_runtime(&app_id) {
        Some(handle) => handle,
        None => {
            return HttpResponse::NotFound()
                .body(format!(r#"{{"error":"app {app_id} not loaded"}}"#));
        }
    };
    let runtime = handle.runtime();

    // Decode authenticated user from ZeroShip-User header (base64 JSON from gateway)
    let user_json = req
        .headers()
        .get("zeroship-user")
        .and_then(|v| v.to_str().ok())
        .and_then(|b64| B64.decode(b64).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok());

    // Set auth user in thread-local before dispatch, clear after
    zeroship_runtime::auth::set_auth_user(user_json);

    // Phase 1: Enter isolate, start dispatch (may return sync or async)
    let outcome = {
        let mut rt = runtime.borrow_mut();
        rt.enter_isolate();
        let o = rt.dispatch_start(handle.modules(), &body);
        rt.exit_isolate();
        o
    };

    // Phase 2: Handle the outcome
    let response = match outcome {
        DispatchOutcome::Complete(Ok(result)) => {
            make_response(&result.json, result.cpu_time.as_secs_f64() * 1000.0)
        }
        DispatchOutcome::Complete(Err(e)) => make_error(&e),

        DispatchOutcome::Pending(rx) => {
            // Async — the pump task will resolve the promise.
            // We yield to compio until the result arrives.
            match recv_with_timeout(&rx, wall_limit(&handle)).await {
                Some(Ok(r)) => make_response(&r.json, r.cpu_time.as_secs_f64() * 1000.0),
                Some(Err(e)) => make_error(&e),
                None => make_error("request timed out"),
            }
        }

        // HTTP handler responses (for onRequest exports)
        DispatchOutcome::HttpComplete { status, headers, body, logs: _ } => {
            make_http_response(status, &headers, &body)
        }
        DispatchOutcome::HttpStream { status, headers, body: reader, logs: _ } => {
            stream_response(status, &headers, reader)
        }
        DispatchOutcome::HttpPending(rx) => {
            match recv_with_timeout(&rx, wall_limit(&handle)).await {
                Some(Ok(zeroship_runtime::HttpDispatchResult::Complete { status, headers, body, .. })) => {
                    make_http_response(status, &headers, &body)
                }
                Some(Ok(zeroship_runtime::HttpDispatchResult::Stream { status, headers, body: reader, .. })) => {
                    stream_response(status, &headers, reader)
                }
                Some(Ok(zeroship_runtime::HttpDispatchResult::WebSocket { .. })) => {
                    make_error("WebSocket upgrade not supported via gateway dispatch")
                }
                Some(Err(e)) => make_error(&e),
                None => make_error("request timed out"),
            }
        }

        DispatchOutcome::WebSocketUpgrade { .. } => {
            make_error("WebSocket upgrade not supported via gateway dispatch")
        }
    };

    // Clear auth user after dispatch (prevent leaking to next request)
    zeroship_runtime::auth::clear_auth_user();

    response
}

fn make_response(json: &str, cpu_ms: f64) -> HttpResponse {
    let mut builder = HttpResponse::Ok();
    builder.content_type("application/json");
    if cpu_ms > 0.0 {
        builder.header("x-cpu-time-ms", format!("{cpu_ms:.2}"));
    }
    builder.body(json.to_string())
}

/// Build an HTTP response forwarding the JS handler's status, headers, and body.
fn make_http_response(status: u16, headers: &[(String, String)], body: &str) -> HttpResponse {
    let status_code = ntex::http::StatusCode::from_u16(status)
        .unwrap_or(ntex::http::StatusCode::OK);
    let mut builder = HttpResponse::build(status_code);
    for (name, value) in headers {
        builder.header(name.as_str(), value.as_str());
    }
    builder.body(body.to_string())
}

/// Build a streaming HTTP response that yields chunks from a V8 ReadableStream
/// in real time. Uses ntex's `streaming()` with an mpsc channel.
///
/// The V8 pump task runs independently (started when the app was loaded),
/// processing fetch callbacks and feeding chunks into the StreamWriter.
/// StreamWriter.push() wakes our drain task via the registered waker —
/// no busy polling.
fn stream_response(
    status: u16,
    headers: &[(String, String)],
    reader: StreamReader,
) -> HttpResponse {
    let status_code = ntex::http::StatusCode::from_u16(status)
        .unwrap_or(ntex::http::StatusCode::OK);
    let mut builder = HttpResponse::build(status_code);
    for (name, value) in headers {
        builder.header(name.as_str(), value.as_str());
    }

    let (tx, rx) = ntex::channel::mpsc::channel();

    // Spawn a drain task — waker-based, not busy-polling.
    // StreamWriter.push() wakes this task when new chunks arrive.
    compio::runtime::spawn(async move {
        loop {
            // Drain all available chunks
            while let Some(chunk) = reader.pop() {
                if !chunk.is_empty() {
                    if tx.send(Ok::<Bytes, std::io::Error>(Bytes::from(chunk))).is_err() {
                        return; // client disconnected
                    }
                }
            }

            // Check if stream is complete
            if reader.is_done() {
                while let Some(chunk) = reader.pop() {
                    if !chunk.is_empty() {
                        let _ = tx.send(Ok(Bytes::from(chunk)));
                    }
                }
                return; // tx drops → stream ends → HTTP response completes
            }

            // Wait for new data (waker-based — no CPU burn)
            // StreamWriter.push() or .close() will wake us
            std::future::poll_fn(|cx| {
                if reader.has_data() || reader.is_done() {
                    std::task::Poll::Ready(())
                } else {
                    reader.register_waker(cx.waker());
                    std::task::Poll::Pending
                }
            }).await;
        }
    }).detach();

    builder.streaming(rx)
}

fn make_error(msg: &str) -> HttpResponse {
    let error = serde_json::json!({
        "jsonrpc": "2.0",
        "error": { "code": -32000, "message": msg },
        "id": null
    });
    HttpResponse::Ok()
        .content_type("application/json")
        .body(serde_json::to_string(&error).unwrap())
}

/// Pull bundle from control plane and load into cache (cold start path).
async fn load_on_demand(config: &WorkerConfig, app_id: &Uuid) -> Result<(), String> {
    let app_version = crate::sync::fetch_app_version(&config.control_url, &config.control_key, app_id).await?;
    let bundle_url = format!("{}/internal/bundles/{}", config.control_url, app_id);
    let bytes = crate::sync::http_get_bytes(&bundle_url, &config.control_key).await?;

    if bytes.is_empty() {
        return Err("empty bundle".into());
    }

    let hash = hex::encode(Sha256::digest(&bytes));

    if cache::load_app(*app_id, &bytes, app_version.runtime) {
        cache::set_hash(*app_id, hash);
        eprintln!("[worker] on-demand loaded {app_id}");
        Ok(())
    } else {
        Err("failed to parse bundle".into())
    }
}
