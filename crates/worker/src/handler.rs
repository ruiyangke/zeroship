use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures::{pin_mut, FutureExt};
use ntex::web::{self, HttpRequest, HttpResponse};
use ntex::util::Bytes;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use zeroship_core::auth::{extract_bearer, validate_control_key, verify_hmac_sha256_hex};
use zeroship_runtime::runtime::DispatchOutcome;
use zeroship_runtime::{ResultReceiver, RuntimeHandle, StreamReader};

use crate::{cache, metrics, WorkerConfig};

/// Verify the gateway-issued bearer token on /dispatch endpoints.
/// Returns `None` if the request is authorized; otherwise a 401 response.
fn check_worker_auth(req: &HttpRequest, worker_key: &str) -> Option<HttpResponse> {
    // Empty worker_key disables the check (dev-only loopback bind enforces this).
    if worker_key.is_empty() {
        return None;
    }
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(extract_bearer);
    match auth {
        Some(token) if validate_control_key(token, worker_key) => None,
        _ => {
            metrics::inc(&metrics::DISPATCH_REJECTED_AUTH);
            Some(HttpResponse::Unauthorized().body(r#"{"error":"unauthorized"}"#))
        }
    }
}

/// Decode a signed `ZeroShip-User` header of form `base64(json).<hex-hmac>`.
/// Returns the inner JSON string only if the HMAC verifies against `worker_key`.
/// When `worker_key` is empty, accepts an unsigned `base64(json)` payload
/// (dev-only loopback bind enforces this).
fn decode_user_header(raw: Option<&str>, worker_key: &str) -> Option<String> {
    let raw = raw?;
    if worker_key.is_empty() {
        // Dev mode: no signature, treat the whole value as base64(json).
        let bytes = B64.decode(raw).ok()?;
        return String::from_utf8(bytes).ok();
    }
    let (b64, mac) = raw.rsplit_once('.')?;
    if !verify_hmac_sha256_hex(worker_key.as_bytes(), b64.as_bytes(), mac) {
        return None;
    }
    let bytes = B64.decode(b64).ok()?;
    String::from_utf8(bytes).ok()
}

/// Wait for a result with a wall-clock timeout. On timeout, trip the
/// `cancel` flag so the runtime drops the pending request and any queued
/// fetches/timers it owns, ping the pump so it runs cleanup immediately,
/// then return `None` to the caller.
///
/// Without this, the worker would send a timeout response to the gateway
/// while the isolate kept running the handler's remaining async work —
/// wasted compute, memory, and outbound network after the client left.
async fn recv_with_timeout<T>(
    rx: &ResultReceiver<T>,
    timeout: std::time::Duration,
    cancel: &zeroship_runtime::CancelFlag,
    handle: &RuntimeHandle,
) -> Option<T> {
    let recv = rx.recv().fuse();
    let sleep = compio::time::sleep(timeout).fuse();
    pin_mut!(recv, sleep);
    futures::select! {
        result = recv => Some(result),
        _ = sleep => {
            cancel.cancel();
            handle.notify_pump();
            None
        }
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
    // Authenticate the gateway before touching the runtime.
    if let Some(resp) = check_worker_auth(&req, &config.worker_key) {
        return resp;
    }

    let app_id = match path.parse::<Uuid>() {
        Ok(id) => id,
        Err(_) => {
            metrics::inc(&metrics::DISPATCH_REJECTED_BAD_APP_ID);
            return HttpResponse::BadRequest().body(r#"{"error":"invalid app_id"}"#);
        }
    };

    // On-demand loading: if app is not cached, pull from control plane
    if cache::get_runtime(&app_id).is_none() {
        metrics::inc(&metrics::ON_DEMAND_LOADS_TOTAL);
        if let Err(e) = load_on_demand(&config, &app_id).await {
            metrics::inc(&metrics::ON_DEMAND_LOAD_FAILURES);
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

    metrics::inc(&metrics::DISPATCH_HTTP_TOTAL);

    // Decode + HMAC-verify the gateway-signed ZeroShip-User header.
    // A forged or tampered header yields None (treated as anonymous).
    //
    // The user travels INTO `dispatch_http` so the runtime can associate
    // it with the specific `request_id` being dispatched. Using a thread
    // local here would leak across `.await` boundaries: handler A yields,
    // handler B sets the thread-local to userB, A's async continuation
    // fires on the pump and reads userB instead of userA.
    let raw_user = req
        .headers()
        .get("zeroship-user")
        .and_then(|v| v.to_str().ok());
    let user_json = decode_user_header(raw_user, &config.worker_key);

    // Parse the HTTP envelope from the request body
    let envelope: HttpEnvelope = match serde_json::from_slice(&body) {
        Ok(env) => env,
        Err(e) => {
            metrics::inc(&metrics::DISPATCH_REJECTED_BAD_ENVELOPE);
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
        let o = rt.dispatch_http(
            handle.modules(),
            &envelope.method,
            &envelope.url,
            &headers_json,
            &envelope.body,
            user_json,
        );
        rt.exit_isolate();
        o
    };

    // Phase 2: Handle the outcome
    let response = match outcome {
        DispatchOutcome::HttpComplete { status, headers, body, logs: _ } => {
            make_http_response(status, headers, body)
        }
        DispatchOutcome::HttpStream { status, headers, body: reader, logs: _ } => {
            stream_response(status, &headers, reader)
        }
        DispatchOutcome::HttpPending { rx, cancel } => {
            match recv_with_timeout(&rx, wall_limit(&handle), &cancel, &handle).await {
                Some(Ok(zeroship_runtime::HttpDispatchResult::Complete { status, headers, body, .. })) => {
                    make_http_response(status, headers, body)
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
            make_response(result.json, result.cpu_time.as_secs_f64() * 1000.0)
        }
        DispatchOutcome::Complete(Err(e)) => make_error(&e),
        DispatchOutcome::Pending { .. } => make_error("unexpected Pending from dispatch_http"),
        DispatchOutcome::WebSocketUpgrade { .. } => {
            make_error("WebSocket upgrade not supported via gateway dispatch")
        }
    };

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
    // Authenticate the gateway before touching the runtime.
    if let Some(resp) = check_worker_auth(&req, &config.worker_key) {
        return resp;
    }

    let app_id = match path.parse::<Uuid>() {
        Ok(id) => id,
        Err(_) => {
            metrics::inc(&metrics::DISPATCH_REJECTED_BAD_APP_ID);
            return HttpResponse::BadRequest().body(r#"{"error":"invalid app_id"}"#);
        }
    };

    // On-demand loading: if app is not cached, pull from control plane
    if cache::get_runtime(&app_id).is_none() {
        metrics::inc(&metrics::ON_DEMAND_LOADS_TOTAL);
        if let Err(e) = load_on_demand(&config, &app_id).await {
            metrics::inc(&metrics::ON_DEMAND_LOAD_FAILURES);
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

    metrics::inc(&metrics::DISPATCH_RPC_TOTAL);

    // Decode + HMAC-verify the gateway-signed ZeroShip-User header.
    // User flows into dispatch_start so it's stored per-request, keyed
    // by request_id — not in a thread-local that async continuations
    // would read at the wrong time.
    let raw_user = req
        .headers()
        .get("zeroship-user")
        .and_then(|v| v.to_str().ok());
    let user_json = decode_user_header(raw_user, &config.worker_key);

    // Phase 1: Enter isolate, start dispatch (may return sync or async)
    let outcome = {
        let mut rt = runtime.borrow_mut();
        rt.enter_isolate();
        let o = rt.dispatch_start(handle.modules(), &body, user_json);
        rt.exit_isolate();
        o
    };

    // Phase 2: Handle the outcome
    let response = match outcome {
        DispatchOutcome::Complete(Ok(result)) => {
            make_response(result.json, result.cpu_time.as_secs_f64() * 1000.0)
        }
        DispatchOutcome::Complete(Err(e)) => make_error(&e),

        DispatchOutcome::Pending { rx, cancel } => {
            // Async — the pump task will resolve the promise.
            // We yield to compio until the result arrives.
            match recv_with_timeout(&rx, wall_limit(&handle), &cancel, &handle).await {
                Some(Ok(r)) => make_response(r.json, r.cpu_time.as_secs_f64() * 1000.0),
                Some(Err(e)) => make_error(&e),
                None => make_error("request timed out"),
            }
        }

        // HTTP handler responses (for onRequest exports)
        DispatchOutcome::HttpComplete { status, headers, body, logs: _ } => {
            make_http_response(status, headers, body)
        }
        DispatchOutcome::HttpStream { status, headers, body: reader, logs: _ } => {
            stream_response(status, &headers, reader)
        }
        DispatchOutcome::HttpPending { rx, cancel } => {
            match recv_with_timeout(&rx, wall_limit(&handle), &cancel, &handle).await {
                Some(Ok(zeroship_runtime::HttpDispatchResult::Complete { status, headers, body, .. })) => {
                    make_http_response(status, headers, body)
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

    response
}

// Takes `json` by value so the bytes of the V8-produced JSON are moved
// straight into the response body. The earlier `&str` signature forced
// `json.to_string()` at every call site — an entire response-body copy
// per request that showed up as consistent per-request allocation cost.
fn make_response(json: String, cpu_ms: f64) -> HttpResponse {
    let mut builder = HttpResponse::Ok();
    builder.content_type("application/json");
    if cpu_ms > 0.0 {
        builder.header("x-cpu-time-ms", format!("{cpu_ms:.2}"));
    }
    builder.body(json)
}

/// Build an HTTP response forwarding the JS handler's status, headers, and body.
///
/// Body is moved, not copied — for large responses (image uploads, large
/// JSON payloads) this halves the memory churn per request.
fn make_http_response(status: u16, headers: Vec<(String, String)>, body: String) -> HttpResponse {
    let status_code = ntex::http::StatusCode::from_u16(status)
        .unwrap_or(ntex::http::StatusCode::OK);
    let mut builder = HttpResponse::build(status_code);
    for (name, value) in &headers {
        builder.header(name.as_str(), value.as_str());
    }
    builder.body(body)
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
