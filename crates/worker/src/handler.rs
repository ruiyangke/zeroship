use std::sync::Arc;

use futures::{pin_mut, FutureExt};
use ntex::web::{self, HttpRequest, HttpResponse};
use ntex::util::Bytes;
use uuid::Uuid;

use zeroship_core::auth::{
    extract_bearer, validate_control_key, verify_zeroship_user_header_for_request,
};
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
pub(crate) fn check_worker_auth(req: &HttpRequest, worker_key: &str) -> Option<HttpResponse> {
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

/// Verify the gateway-signed `ZeroShip-User` header. On success returns
/// `(decoded_json, raw_signed_header)`:
///   - `decoded_json` is what `env.auth.getUser()` returns;
///   - `raw_signed_header` is the VERBATIM `base64(JSON).<rid>.<iat>.<hmac>`
///     string. The runtime stashes it Rust-side so the power-token op
///     (`env.auth.getAccessToken`) can echo it to control for stateless
///     re-verification (R4). It is never exposed to app JS.
fn verified_user_json(
    req: &HttpRequest,
    worker_key: &str,
) -> Result<Option<(String, String)>, HttpResponse> {
    let Some(value) = req.headers().get("zeroship-user") else {
        return Ok(None);
    };
    let Ok(header) = value.to_str() else {
        metrics::inc(&metrics::DISPATCH_REJECTED_AUTH);
        return Err(HttpResponse::Unauthorized().body(r#"{"error":"invalid user header"}"#));
    };
    let expected_request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| Uuid::parse_str(v).ok());
    let Some(expected_request_id) = expected_request_id else {
        metrics::inc(&metrics::DISPATCH_REJECTED_AUTH);
        return Err(HttpResponse::Unauthorized().body(r#"{"error":"invalid user header"}"#));
    };
    match verify_zeroship_user_header_for_request(
        worker_key.as_bytes(),
        header,
        expected_request_id,
    ) {
        Some(json) => Ok(Some((json, header.to_string()))),
        None => {
            metrics::inc(&metrics::DISPATCH_REJECTED_AUTH);
            Err(HttpResponse::Unauthorized().body(r#"{"error":"invalid user header"}"#))
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

/// JSON envelope the gateway sends. The full HTTP request (method, URL,
/// headers, body) flows in here and `Runtime::call_fetch_handler`
/// dispatches it through the kernel's three-tier path:
///   1. `default.rpc(name, input, ctx)` for `/_zs/v1/<id>` URLs.
///   2. `default.fetchFast(method, url, body, env)` for non-RPC traffic.
///   3. `default.fetch(request, env, ctx)` (WinterCG slow path) for
///      everything else, including fall-through from (1) and (2).
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
    logs: web::types::State<crate::logs::SharedLogs>,
    path: web::types::Path<String>,
    body: Bytes,
) -> HttpResponse {
    // Authenticate the gateway before touching the runtime.
    if let Some(resp) = check_worker_auth(&req, &config.worker_key) {
        return resp;
    }
    let (user_json, user_header) = match verified_user_json(&req, &config.worker_key) {
        Ok(Some((json, header))) => (Some(json), Some(header)),
        Ok(None) => (None, None),
        Err(resp) => return resp,
    };

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
        let o = runtime.call_fetch_handler_with_user(
            &envelope.method,
            &envelope.url,
            &envelope.headers,
            &envelope.body,
            &env,
            ctx,
            user_json,
            user_header,
        );
        runtime.exit_isolate();
        o
    };

    match outcome {
        FetchOutcome::Response { status, headers, body, logs: request_logs } => {
            crate::logs::append(&logs, app_id, request_logs);
            make_http_response(status, headers, body)
        }
        FetchOutcome::Stream { status, headers, body_reader, logs: request_logs } => {
            crate::logs::append(&logs, app_id, request_logs);
            stream_response(status, &headers, body_reader)
        }
        FetchOutcome::WebSocketUpgrade { .. } => {
            // WS upgrades over the HTTP dispatch endpoint aren't supported —
            // the gateway uses a separate WS proxy path for websocket traffic.
            make_error_msg(500, "WebSocket upgrade not supported via HTTP dispatch")
        }
        FetchOutcome::Pending { rx, cancel: cf } => {
            match recv_with_timeout(&rx, wall_limit(&runtime), &cf, &runtime).await {
                Some(Ok(SettledFetch::Response {
                    status,
                    headers,
                    body,
                    logs: request_logs,
                })) => {
                    crate::logs::append(&logs, app_id, request_logs);
                    make_http_response(status, headers, body)
                }
                Some(Ok(SettledFetch::Stream {
                    status,
                    headers,
                    body_reader,
                    logs: request_logs,
                })) => {
                    crate::logs::append(&logs, app_id, request_logs);
                    stream_response(status, &headers, body_reader)
                }
                Some(Ok(SettledFetch::WebSocketUpgrade { logs: request_logs, .. })) => {
                    crate::logs::append(&logs, app_id, request_logs);
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Once, RwLock};

    use ntex::http::StatusCode;
    use ntex::web::{self, test};
    use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
    use zeroship_core::types::AppRuntimeLimits;
    use zeroship_runtime::init::init_v8;

    use super::*;

    static V8_INIT: Once = Once::new();

    fn init_runtime() {
        V8_INIT.call_once(init_v8);
    }

    fn tmpdir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "zs-worker-logs-{label}-{}",
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).expect("mkdir tmp");
        path
    }

    #[test]
    fn dispatch_console_lines_are_queryable_from_logs_endpoint() {
        let Ok(runtime) = compio::runtime::Runtime::new() else {
            eprintln!("skipping (cannot create compio runtime)");
            return;
        };

        runtime.block_on(async {
            init_runtime();

            let app_id = Uuid::new_v4();
            let source = br#"
                export default {
                  fetch(req) {
                    console.log("b2-real-log", new URL(req.url).pathname);
                    return new Response("ok");
                  }
                }
            "#;
            crate::cache::init_cache(
                10,
                crate::cache::KernelConfig {
                    db_url: None,
                    kv_url: None,
                    storage_root: None,
                    control_url: String::new(),
                    control_key: String::new(),
                },
            );
            assert!(crate::cache::load_app(
                app_id,
                source,
                AppRuntimeLimits::default()
            ));

            let envs: SharedEnvs = Arc::new(RwLock::new(HashMap::new()));
            crate::sync::put_env_from_json(
                &envs,
                app_id,
                r#"{"vars":{},"secrets":{},"expose":[]}"#,
                0,
            )
            .expect("insert env");
            let logs = crate::logs::new_store();
            let blob_root = tmpdir("blob");
            let blob_store: Arc<dyn BlobStore> =
                Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
            let config = Arc::new(crate::WorkerConfig {
                control_url: "http://127.0.0.1:1".to_string(),
                control_key: String::new(),
                db_url: None,
                kv_url: None,
                storage_root: None,
                max_isolates: 10,
                poll_interval_secs: 60,
                worker_key: String::new(),
                shutdown_timeout_secs: 0,
                blob_store,
            });

            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .service(web::resource("/dispatch/{app_id}").route(web::post().to(dispatch)))
                    .service(
                        web::resource("/logs/{app_id}")
                            .route(web::get().to(crate::logs::get_logs)),
                    ),
            )
            .await;

            let envelope = serde_json::json!({
                "method": "GET",
                "url": "http://example.test/from-worker-test",
                "headers": [],
                "body": "",
            });
            let req = test::TestRequest::post()
                .uri(&format!("/dispatch/{app_id}"))
                .set_payload(serde_json::to_vec(&envelope).unwrap())
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            assert_eq!(&body[..], b"ok");

            let req = test::TestRequest::get()
                .uri(&format!("/logs/{app_id}"))
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            let lines: Vec<String> = serde_json::from_slice(&body).expect("logs json");
            assert_eq!(lines, vec!["b2-real-log /from-worker-test"]);

            let _ = std::fs::remove_dir_all(blob_root);
        });
    }

    /// Phase-2 faithful regression: a deployed app must boot against the
    /// FULL `env.{db,kv,storage,auth}` kernel on the multi-node worker
    /// path — not just `env.{db,auth}`. This drives the REAL dispatch
    /// pipeline an end-user request takes: `init_cache` → `load_app`
    /// (which calls `create_plugins()` — the very vector a deployed app
    /// gets) → ntex `/dispatch/{app_id}` → `call_fetch_handler`. There is
    /// NO hand-built plugin list and NO dispatcher shim; the JS handler
    /// runs inside the isolate `create_plugins()` actually feeds.
    ///
    /// The handler asserts all four namespaces RESOLVE, then round-trips
    /// `env.kv.set/get` (against the real `Redis` backend `create_plugins`
    /// builds) and `env.storage.put/get` (against the real `LocalFs`
    /// backend). `env.db` is asserted present (its pool connects lazily, so
    /// no live Postgres is needed to prove the namespace is installed);
    /// `env.auth.getUser()` resolves to null with no user header.
    ///
    /// PRE-PHASE-2 this FAILS: the old `create_plugins()` pushed only
    /// `DbPlugin` + `AuthPlugin`, so `typeof env.kv` / `typeof env.storage`
    /// were `"undefined"` and the handler's assertion would 500.
    ///
    /// Service dependency: a single-node Redis reachable at `REDIS_TEST_URL`
    /// (e.g. `redis://127.0.0.1:6379`). When unset the KV leg can't run
    /// faithfully, so the test SKIPS (matching `plugin-kv`'s
    /// `tests/redis_backend.rs`); set `KV_REQUIRE_REDIS=1` to turn the skip
    /// into a hard failure in CI. Storage + db + auth need no external
    /// service.
    #[test]
    fn dispatch_resolves_full_kernel_kv_storage_db_auth() {
        let Some(kv_url) = std::env::var("REDIS_TEST_URL").ok().filter(|s| !s.is_empty())
        else {
            if std::env::var("KV_REQUIRE_REDIS").ok().as_deref() == Some("1") {
                panic!("KV_REQUIRE_REDIS=1 but REDIS_TEST_URL is unset");
            }
            eprintln!(
                "skipping dispatch_resolves_full_kernel_kv_storage_db_auth \
                 (set REDIS_TEST_URL=redis://127.0.0.1:6379)"
            );
            return;
        };

        let Ok(runtime) = compio::runtime::Runtime::new() else {
            eprintln!("skipping (cannot create compio runtime)");
            return;
        };

        runtime.block_on(async {
            init_runtime();

            let app_id = Uuid::new_v4();
            // The handler exercises every kernel namespace and reports a
            // JSON verdict. A missing namespace surfaces as a thrown
            // TypeError (→ 500), so a green 200 + matching body proves the
            // full kernel resolved AND the kv/storage round-trips landed.
            let source = br#"
                export default {
                  async fetch(req, env) {
                    const present = {
                      db: typeof env.db,
                      kv: typeof env.kv,
                      storage: typeof env.storage,
                      auth: typeof env.auth,
                    };
                    // KV round-trip: set then read back.
                    await env.kv.set("phase2-key", "phase2-value");
                    const kvBack = await env.kv.get("phase2-key");
                    // Storage round-trip: put base64 "hi" then read it back.
                    // The native env.storage.* primitive returns a JSON
                    // STRING (the @zeroship/storage SDK parses it); mirror
                    // that here so the test exercises the real contract.
                    const b64 = btoa("hi");
                    await env.storage.put("uploads", "f.txt", b64, "text/plain");
                    const raw = await env.storage.get("uploads", "f.txt");
                    const got = raw ? JSON.parse(raw) : null;
                    // Auth resolves (null with no user header).
                    const user = await env.auth.getUser();
                    return Response.json({
                      present,
                      kvBack,
                      storageBack: got ? got.bytesBase64 : null,
                      user,
                    });
                  }
                }
            "#;

            let storage_root = tmpdir("storage");
            crate::cache::init_cache(
                10,
                crate::cache::KernelConfig {
                    // Dummy DSN: DbPlugin stores the URL and connects lazily,
                    // so `env.db` is installed without a live Postgres.
                    db_url: Some("postgres://localhost/zs_phase2_unused".to_string()),
                    kv_url: Some(kv_url),
                    storage_root: Some(storage_root.clone()),
                    control_url: String::new(),
                    control_key: String::new(),
                },
            );
            assert!(crate::cache::load_app(
                app_id,
                source,
                AppRuntimeLimits::default()
            ));

            let envs: SharedEnvs = Arc::new(RwLock::new(HashMap::new()));
            crate::sync::put_env_from_json(
                &envs,
                app_id,
                r#"{"vars":{},"secrets":{},"expose":[]}"#,
                0,
            )
            .expect("insert env");
            let logs = crate::logs::new_store();
            let blob_root = tmpdir("blob");
            let blob_store: Arc<dyn BlobStore> =
                Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
            let config = Arc::new(crate::WorkerConfig {
                control_url: "http://127.0.0.1:1".to_string(),
                control_key: String::new(),
                db_url: Some("postgres://localhost/zs_phase2_unused".to_string()),
                kv_url: None,
                storage_root: None,
                max_isolates: 10,
                poll_interval_secs: 60,
                worker_key: String::new(),
                shutdown_timeout_secs: 0,
                blob_store,
            });

            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .service(
                        web::resource("/dispatch/{app_id}").route(web::post().to(dispatch)),
                    ),
            )
            .await;

            let envelope = serde_json::json!({
                "method": "GET",
                "url": "http://example.test/kernel-probe",
                "headers": [],
                "body": "",
            });
            let req = test::TestRequest::post()
                .uri(&format!("/dispatch/{app_id}"))
                .set_payload(serde_json::to_vec(&envelope).unwrap())
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "full-kernel dispatch must succeed; a missing env.* namespace 500s"
            );
            let body = test::read_body(resp).await;
            let v: serde_json::Value =
                serde_json::from_slice(&body).expect("handler returned JSON");

            // All four namespaces resolved to live objects.
            assert_eq!(v["present"]["db"], "object", "env.db must resolve");
            assert_eq!(v["present"]["kv"], "object", "env.kv must resolve");
            assert_eq!(
                v["present"]["storage"], "object",
                "env.storage must resolve"
            );
            assert_eq!(v["present"]["auth"], "object", "env.auth must resolve");
            // KV set/get round-tripped through the real Redis backend.
            assert_eq!(v["kvBack"], "phase2-value", "kv round-trip");
            // Storage put/get round-tripped through the real LocalFs backend.
            assert_eq!(
                v["storageBack"],
                base64_encode_hi(),
                "storage round-trip (base64 of \"hi\")"
            );
            // Auth resolved (no user header → null).
            assert!(v["user"].is_null(), "auth.getUser() returns null sans user");

            let _ = std::fs::remove_dir_all(blob_root);
            let _ = std::fs::remove_dir_all(storage_root);
        });
    }

    /// `btoa("hi")` — the base64 the JS handler stores. Kept Rust-side so
    /// the assertion can't drift from the handler's literal.
    fn base64_encode_hi() -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(b"hi")
    }
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
/// Bytes come from `BlobStore` keyed by
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
    tracing::info!(
        app_id = %app_id,
        blob_prefix = &bundle_hash[..bundle_hash.len().min(8)],
        "worker: on-demand loaded app"
    );
    Ok(())
}
