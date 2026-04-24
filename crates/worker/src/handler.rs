use std::sync::Arc;

use futures::{pin_mut, FutureExt};
use ntex::web::{self, HttpRequest, HttpResponse};
use ntex::util::Bytes;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use zeroship_core::auth::{extract_bearer, validate_control_key};
use zeroship_runtime::runtime::DispatchError;
use zeroship_runtime::{
    CancelFlag, EnvSnapshot, FetchOutcome, RequestCtx, ResultReceiver, Runtime, SettledFetch,
    StreamReader,
};

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
    cancel: &CancelFlag,
    runtime: &Runtime,
) -> Option<T> {
    let recv = rx.recv().fuse();
    let sleep = compio::time::sleep(timeout).fuse();
    pin_mut!(recv, sleep);
    futures::select! {
        result = recv => Some(result),
        _ = sleep => {
            cancel.cancel();
            runtime.notify_pump();
            None
        }
    }
}

fn wall_limit(runtime: &Runtime) -> std::time::Duration {
    runtime
        .wall_timeout()
        .unwrap_or(std::time::Duration::from_secs(30))
}

// ---------------------------------------------------------------------------
// Unified dispatch — the worker's single entry point
// ---------------------------------------------------------------------------

/// JSON envelope the gateway sends. The kernel no longer distinguishes RPC
/// from HTTP; the full HTTP request (method, URL, headers, body) flows in
/// via this envelope and `call_fetch_handler` invokes the app's
/// `default.fetch` handler. When the bootstrap router lands (PR 2), it
/// inspects the URL inside the envelope to dispatch to `_rpc/<method>`,
/// static assets, or the app's own fetch — all from user-space JS.
#[derive(serde::Deserialize)]
struct HttpEnvelope {
    method: String,
    url: String,
    /// Headers as [[key, value], ...] array.
    headers: Vec<(String, String)>,
    #[serde(default)]
    body: String,
}

/// Dispatch an HTTP request through the V8 fetch handler.
///
/// The gateway forwards an HTTP envelope (method, URL, headers, body) and
/// the worker hands it to `Runtime::call_fetch_handler`, which invokes the
/// app's exported `default.fetch(req, env, ctx)`. Response may be buffered
/// or streaming (SSE); WebSocket upgrades aren't reachable through this
/// endpoint (the gateway uses a separate WS proxy path).
pub async fn dispatch(
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

    // On-demand loading: if app is not cached, pull from control plane.
    if cache::get_runtime(&app_id).is_none() {
        metrics::inc(&metrics::ON_DEMAND_LOADS_TOTAL);
        if let Err(e) = load_on_demand(&config, &app_id).await {
            metrics::inc(&metrics::ON_DEMAND_LOAD_FAILURES);
            return HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": format!("failed to load app: {e}")}));
        }
    }

    let runtime = match cache::get_runtime(&app_id) {
        Some(r) => r,
        None => {
            return HttpResponse::NotFound()
                .body(format!(r#"{{"error":"app {app_id} not loaded"}}"#));
        }
    };

    metrics::inc(&metrics::DISPATCH_TOTAL);

    // Parse the HTTP envelope from the request body.
    let envelope: HttpEnvelope = match serde_json::from_slice(&body) {
        Ok(env) => env,
        Err(e) => {
            metrics::inc(&metrics::DISPATCH_REJECTED_BAD_ENVELOPE);
            return HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": format!("invalid envelope: {e}")}));
        }
    };

    // Build env + ctx. env comes from the per-thread cache, populated
    // alongside the bundle on load / version bump. If the env fetch
    // failed (e.g., control unreachable), we fall back to empty rather
    // than refusing the request — the handler can still do useful work
    // without creator-supplied bindings.
    let env = match cache::get_env(&app_id) {
        Some(json) => match serde_json::from_str::<serde_json::Value>(&json) {
            Ok(v) => EnvSnapshot::new(v),
            Err(e) => {
                eprintln!("[worker] bad env json for {app_id}: {e}; using empty");
                EnvSnapshot::empty()
            }
        },
        None => EnvSnapshot::empty(),
    };
    let cancel = CancelFlag::new();
    let ctx = RequestCtx::new(cancel.clone());

    // Enter isolate, dispatch through the unified fetch handler.
    let outcome = {
        runtime.enter_isolate();
        let o = runtime.call_fetch_handler(
            &envelope.method,
            &envelope.url,
            &envelope.headers,
            &envelope.body,
            &env,
            ctx,
        );
        runtime.exit_isolate();
        o
    };

    match outcome {
        FetchOutcome::Response { status, headers, body, logs: _ } => {
            make_http_response(status, headers, body)
        }
        FetchOutcome::Stream { status, headers, body_reader, logs: _ } => {
            stream_response(status, &headers, body_reader)
        }
        FetchOutcome::WebSocketUpgrade { .. } => {
            // WS upgrades over the HTTP dispatch endpoint aren't supported —
            // the gateway uses a separate WS proxy path for websocket traffic.
            make_error_msg(500, "WebSocket upgrade not supported via HTTP dispatch")
        }
        FetchOutcome::Pending { rx, cancel: cf } => {
            match recv_with_timeout(&rx, wall_limit(&runtime), &cf, &runtime).await {
                Some(Ok(SettledFetch::Response { status, headers, body, .. })) => {
                    make_http_response(status, headers, body)
                }
                Some(Ok(SettledFetch::Stream { status, headers, body_reader, .. })) => {
                    stream_response(status, &headers, body_reader)
                }
                Some(Ok(SettledFetch::WebSocketUpgrade { .. })) => {
                    make_error_msg(500, "WebSocket upgrade not supported via HTTP dispatch")
                }
                Some(Err(e)) => make_error(&e),
                None => make_error_msg(504, "request timed out"),
            }
        }
    }
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

fn make_error(err: &DispatchError) -> HttpResponse {
    // Wire: `{"message","name"}` body. Status comes from DispatchError so
    // JS-thrown errors with `err.status` (e.g. 400 for bad input) reach
    // the client instead of being flattened to 500.
    let body = serde_json::json!({ "message": err.message, "name": "Error" });
    let status = ntex::http::StatusCode::from_u16(err.status)
        .unwrap_or(ntex::http::StatusCode::INTERNAL_SERVER_ERROR);
    HttpResponse::build(status)
        .content_type("application/json")
        .body(serde_json::to_string(&body).unwrap())
}

fn make_error_msg(status: u16, msg: &str) -> HttpResponse {
    make_error(&DispatchError::new(msg, status))
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
        // Fetch env alongside the bundle. Env failures don't block the
        // cold-start — the handler falls back to EnvSnapshot::empty.
        match crate::sync::fetch_app_env(&config.control_url, &config.control_key, app_id).await {
            Ok(env_json) => cache::set_env(*app_id, env_json),
            Err(e) => eprintln!("[worker] cold-start env fetch for {app_id}: {e}"),
        }
        eprintln!("[worker] on-demand loaded {app_id}");
        Ok(())
    } else {
        Err("failed to parse bundle".into())
    }
}
