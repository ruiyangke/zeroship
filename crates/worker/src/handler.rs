use std::sync::Arc;

use futures::{pin_mut, FutureExt};
use ntex::web::{self, HttpRequest, HttpResponse};
use ntex::util::Bytes;
use uuid::Uuid;

use zeroship_core::auth::{extract_bearer, validate_control_key};
use zeroship_runtime::runtime::DispatchError;
use zeroship_runtime::{
    CancelFlag, EnvSnapshot, FetchOutcome, RequestCtx, ResultReceiver, Runtime, SettledFetch,
    StreamReader,
};

use crate::sync::SharedEnvs;
use crate::{cache, metrics, WorkerConfig};

/// Cap on the dispatch envelope body. The envelope wraps a creator-app
/// HTTP request including headers and body — most apps don't need
/// huge inbound bodies on this surface (file uploads typically go
/// straight to object storage). 4 MiB is generous enough for JSON
/// APIs + form posts and small enough to bound per-request memory.
pub const MAX_DISPATCH_BODY_BYTES: usize = 4 * 1024 * 1024;

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
    envs: web::types::State<SharedEnvs>,
    path: web::types::Path<String>,
    body: Bytes,
) -> HttpResponse {
    // Authenticate the gateway before touching the runtime.
    if let Some(resp) = check_worker_auth(&req, &config.worker_key) {
        return resp;
    }

    // Cheap rejection BEFORE app_id parse / runtime lookup / env load.
    if body.len() > MAX_DISPATCH_BODY_BYTES {
        metrics::inc(&metrics::DISPATCH_REJECTED_BODY_TOO_LARGE);
        return HttpResponse::PayloadTooLarge()
            .json(&serde_json::json!({"error": "dispatch body too large"}));
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
        if let Err(e) = load_on_demand(&config, &envs, &app_id).await {
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

    // env comes from the process-wide SharedEnvs (Arc<RwLock>), populated
    // by the reconcile loop or load-on-demand path. Read = brief read
    // lock + Arc clone; never held across await.
    //
    // Fail-closed: if env is missing we return 503 rather than serve
    // the app with empty bindings. A creator app keying authorization
    // off `env.ADMIN_TOKEN` presence would otherwise silently
    // fail-open.
    // env clone is the EnvSnapshot inside the cached entry. The
    // CachedEnv wrapper carries the version for cross-thread dedup
    // in the reconcile path; the dispatch hot path just needs the
    // snapshot.
    let env: EnvSnapshot = match crate::sync::get_env(&envs, &app_id) {
        Some(entry) => entry.snapshot.clone(),
        None => {
            metrics::inc(&metrics::ENV_UNAVAILABLE_TOTAL);
            return HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": "env unavailable"}));
        }
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
    metrics::inc(&metrics::DISPATCH_ERRORS_TOTAL);
    make_error(&DispatchError::new(msg, status))
}

/// Pull bundle from the blob store and load into cache (cold start path).
///
/// Order matters: fetch + verify bundle, fetch + parse env, THEN
/// commit V8 isolate + env atomically. Doing it in the other order
/// (commit isolate, then fetch env) would expose a window where
/// `cache::get_runtime` returns a ready isolate but `get_env` returns
/// None — concurrent dispatches on the same thread between the two
/// steps would 503 unnecessarily AND the new V8 code might run
/// against the OLD env on a later step in this function. Now we
/// stage everything in locals first and only mutate cache at the
/// end.
///
/// Phase 4b: bytes come from `BlobStore` keyed by
/// `manifest.worker.modules[manifest.worker.entry]` rather than from a
/// dedicated control-plane endpoint. The blob store enforces
/// `sha256(bytes) == hash` on read, so the previous explicit hash
/// re-check is redundant — `LocalDiskBlobStore::get_blob` already
/// rejects on mismatch.
async fn load_on_demand(
    config: &WorkerConfig,
    envs: &SharedEnvs,
    app_id: &Uuid,
) -> Result<(), String> {
    let app_version = crate::sync::fetch_app_version(&config.control_url, &config.control_key, app_id).await?;

    let manifest = app_version
        .manifest
        .as_ref()
        .ok_or_else(|| format!("app {app_id} has no manifest yet"))?;
    let bundle_hash = crate::sync::worker_entry_hash(manifest, app_id)
        .ok_or_else(|| format!("app {app_id} has no worker code (SSG-only or malformed manifest)"))?;

    let bytes = config
        .blob_store
        .get_blob(&bundle_hash)
        .await
        .map_err(|e| format!("blob fetch failed: {e}"))?;

    if bytes.is_empty() {
        return Err("empty bundle".into());
    }

    // Fetch env BEFORE committing the V8 isolate. If env fetch fails
    // we never partially-load.
    let env_json = crate::sync::fetch_app_env(&config.control_url, &config.control_key, app_id)
        .await
        .map_err(|e| format!("env fetch failed: {e}"))?;

    // Now commit both atomically (env first so dispatchers always see
    // env present once runtime is present).
    if let Err(e) = crate::sync::put_env_from_json(envs, *app_id, &env_json, app_version.env_version) {
        return Err(format!("env parse failed: {e}"));
    }
    if !cache::load_app(*app_id, &bytes, app_version.runtime.clone()) {
        crate::sync::remove_env(envs, app_id);
        return Err("failed to parse bundle".into());
    }
    // Track the deploy_hash (if any) so the reconcile loop can detect
    // future swaps. Synthesize-on-load: we have bundle_hash here, but
    // the worker's reconcile compares against `info.deploy_hash` (the
    // canonical manifest hash), not the per-blob hash, so use that.
    if let Some(dh) = app_version.deploy_hash.clone() {
        cache::set_hash(*app_id, dh);
    }
    eprintln!(
        "[worker] on-demand loaded {app_id} (blob: {}...)",
        &bundle_hash[..bundle_hash.len().min(8)]
    );
    Ok(())
}
