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

/// Streaming-usage incremental-flush cadence (metering coverage #27, H1).
///
/// The streaming drain records an `egress_bytes` + `stream_wall_us` delta
/// whenever EITHER threshold is crossed, so a long-lived SSE/streaming
/// response bills continuously and a worker crash loses at most one
/// interval's delta. These are RECORDING-cadence knobs (crash-loss
/// granularity), distinct from the meter's flush-to-control cadence
/// (`DEFAULT_FLUSH_INTERVAL`). The interval matches the flush cadence so a
/// recorded delta is rarely stranded in-memory more than one flush.
const STREAM_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
/// ~1 MiB bounds the in-memory un-recorded egress between deltas.
const STREAM_FLUSH_BYTES: u64 = 1024 * 1024;

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

fn verified_user_json(req: &HttpRequest, worker_key: &str) -> Result<Option<String>, HttpResponse> {
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
        Some(json) => Ok(Some(json)),
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
    timeout: Option<std::time::Duration>,
    cancel: &CancelFlag,
    runtime: &Runtime,
) -> Option<T> {
    // No wall cap (the `unlimited`/`enterprise` plan reports `wall_timeout =
    // None`): await the result indefinitely. A genuinely long request — a
    // multi-GB streaming `env.storage` upload, say — must not be cut, which is
    // exactly what the plan's opt-out promises. (Bounded plans still pass a
    // `Some(_)` deadline below.)
    let Some(timeout) = timeout else {
        return Some(rx.recv().await);
    };
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

/// The dispatch wall cap. `None` for the `unlimited`/`enterprise` plan
/// (`wall_timeout` unset) ⇒ no cap. Previously this `unwrap_or(30s)`'d the
/// `None`, silently capping every request at 30 s even on the unlimited plan —
/// which made large single-request streaming uploads impossible regardless of
/// plan. Bounded plans keep their configured `Some(_)` deadline.
fn wall_limit(runtime: &Runtime) -> Option<std::time::Duration> {
    runtime.wall_timeout()
}

// ---------------------------------------------------------------------------
// Unified dispatch — the worker's single entry point
// ---------------------------------------------------------------------------

/// JSON envelope the gateway sends. The full HTTP request (method, URL,
/// headers, body) flows in here and `Runtime::call_fetch_handler`
/// dispatches it through the kernel's three-tier path:
///   1. `default.rpc(name, input, ctx)` for `/__zeroship/v1/<id>` URLs.
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
    let user_json = match verified_user_json(&req, &config.worker_key) {
        Ok(user_json) => user_json,
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

    // Metering auto-counters: start the wall clock now so it spans the whole
    // dispatch (V8 entry + any pending-promise await). The full five-counter
    // record happens once at the end of dispatch, when egress is known — see
    // `cache::record_request` below.
    let wall_start = std::time::Instant::now();

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

    // ingress_bytes = the end-user request body bytes the worker received
    // (the inner envelope body, not the JSON envelope wrapper overhead).
    let ingress_bytes = envelope.body.len() as u64;

    // Enter isolate, dispatch through the unified fetch handler. Sample the
    // V8 thread's CPU clock (CLOCK_THREAD_CPUTIME_ID — the same clock the
    // CPU limiter arms) around the synchronous isolate entry: the delta is
    // the real CPU time this request burned in V8. (For a `Pending` handler
    // the async continuation runs on the shared V8 actor thread via the
    // pump and is not attributable to this request without a per-request
    // accumulator the kernel does not expose; the synchronous burn captured
    // here is the faithful, non-fabricated lower bound — see report.)
    let cpu_start = zeroship_runtime::init::thread_cpu_time();
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
        );
        runtime.exit_isolate();
        o
    };
    let cpu_us = zeroship_runtime::init::thread_cpu_time()
        .saturating_sub(cpu_start)
        .as_micros() as u64;

    // Record all five platform auto-counters once `egress_bytes` is known.
    // For a buffered response that is the body length, recorded inline here;
    // for a streaming response the body bytes aren't known until the stream
    // drains, so the recording is deferred into the drain task.
    let record = |egress_bytes: u64| {
        let wall_us = wall_start.elapsed().as_micros() as u64;
        cache::record_request(&app_id, cpu_us, wall_us, egress_bytes, ingress_bytes);
    };

    match outcome {
        FetchOutcome::Response { status, headers, body, logs: request_logs } => {
            crate::logs::append(&logs, app_id, request_logs);
            record(body.len() as u64);
            make_http_response(status, headers, body)
        }
        FetchOutcome::Stream { status, headers, body_reader, logs: request_logs } => {
            crate::logs::append(&logs, app_id, request_logs);
            record_stream_unary(app_id, cpu_us, ingress_bytes, wall_start);
            stream_response(status, &headers, body_reader, app_id)
        }
        FetchOutcome::WebSocketUpgrade { .. } => {
            // WS upgrades over the HTTP dispatch endpoint aren't supported —
            // the gateway uses a separate WS proxy path for websocket traffic.
            record(0);
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
                    record(body.len() as u64);
                    make_http_response(status, headers, body)
                }
                Some(Ok(SettledFetch::Stream {
                    status,
                    headers,
                    body_reader,
                    logs: request_logs,
                })) => {
                    crate::logs::append(&logs, app_id, request_logs);
                    record_stream_unary(app_id, cpu_us, ingress_bytes, wall_start);
                    stream_response(status, &headers, body_reader, app_id)
                }
                Some(Ok(SettledFetch::WebSocketUpgrade { logs: request_logs, .. })) => {
                    crate::logs::append(&logs, app_id, request_logs);
                    record(0);
                    make_error_msg(500, "WebSocket upgrade not supported via HTTP dispatch")
                }
                Some(Err(e)) => {
                    record(0);
                    make_error(&e)
                }
                None => {
                    record(0);
                    make_error_msg(504, "request timed out")
                }
            }
        }
    }
}

/// Record the UNARY metering counters for a streaming response EXACTLY ONCE,
/// at stream start (metering coverage #27, H1).
///
/// A stream is one request, so `requests`/`cpu_us`/`ingress_bytes` (and the
/// unary `wall_us` — the synchronous-handler elapsed up to stream start) are
/// recorded here a single time, immediately — not deferred to finalize where
/// a crash would lose them. `egress_bytes` is recorded as 0 here; the
/// streamed body bytes and the held-open duration (`stream_wall_us`) accrue
/// INCREMENTALLY in the drain task via [`cache::record_stream_delta`], so a
/// multi-hour stream bills continuously and a crash loses ≤ one interval.
fn record_stream_unary(
    app_id: Uuid,
    cpu_us: u64,
    ingress_bytes: u64,
    wall_start: std::time::Instant,
) {
    let wall_us = wall_start.elapsed().as_micros() as u64;
    // egress = 0: the streamed body accrues as incremental `egress_bytes`
    // deltas in the drain; `requests` is counted once here and never again.
    cache::record_request(&app_id, cpu_us, wall_us, 0, ingress_bytes);
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
    app_id: Uuid,
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
    //
    // Metering coverage (#27, H1): the drain records `egress_bytes` +
    // `stream_wall_us` INCREMENTALLY — a delta whenever ≥ STREAM_FLUSH_BYTES
    // have streamed OR ≥ STREAM_FLUSH_INTERVAL has elapsed, plus a final
    // delta when the loop ends (client disconnect or stream complete). So a
    // multi-hour stream bills continuously and a worker crash loses at most
    // one interval's delta, instead of the whole stream (the pre-fix
    // finalize-only behaviour). `requests`/`cpu_us`/`ingress_bytes` were
    // already recorded once at stream start (`record_stream_unary`) and are
    // NOT touched here — a stream is one request.
    compio::runtime::spawn(async move {
        let stream_start = std::time::Instant::now();
        // Running deltas SINCE the last recorded flush.
        let mut bytes_since_flush: u64 = 0;
        // Stream-wall already recorded (so each delta is `elapsed - recorded`).
        let mut wall_recorded_us: u64 = 0;

        // Record a delta of egress + stream-wall since the last flush, then
        // reset the byte counter and advance the recorded-wall marker.
        macro_rules! flush_delta {
            () => {{
                let elapsed_us = stream_start.elapsed().as_micros() as u64;
                let wall_delta = elapsed_us.saturating_sub(wall_recorded_us);
                cache::record_stream_delta(&app_id, bytes_since_flush, wall_delta);
                bytes_since_flush = 0;
                wall_recorded_us = elapsed_us;
            }};
        }

        loop {
            // Drain all available chunks. The byte-threshold flush is checked
            // INSIDE the loop so a burst of many queued chunks can't overshoot
            // the ~STREAM_FLUSH_BYTES crash-loss bound by an unbounded amount —
            // we flush as soon as the accrued delta crosses the threshold,
            // mid-burst, rather than only once after draining everything.
            while let Some(chunk) = reader.pop() {
                if !chunk.is_empty() {
                    bytes_since_flush += chunk.len() as u64;
                    if tx.send(Ok::<Bytes, std::io::Error>(Bytes::from(chunk))).is_err() {
                        flush_delta!(); // client disconnected — land the trailing delta
                        return;
                    }
                    if bytes_since_flush >= STREAM_FLUSH_BYTES {
                        flush_delta!();
                    }
                }
            }

            // Check if stream is complete
            if reader.is_done() {
                while let Some(chunk) = reader.pop() {
                    if !chunk.is_empty() {
                        bytes_since_flush += chunk.len() as u64;
                        let _ = tx.send(Ok(Bytes::from(chunk)));
                    }
                }
                flush_delta!(); // final delta
                return; // tx drops → stream ends → HTTP response completes
            }

            // Wait for new data (waker-based — no CPU burn), but bounded by
            // STREAM_FLUSH_INTERVAL so an idle-but-open stream still wakes to
            // record its held-open duration (and a crash bounds loss to one
            // interval). The waker (StreamWriter.push()/.close()) wins when
            // data arrives sooner.
            let wait = std::future::poll_fn(|cx| {
                if reader.has_data() || reader.is_done() {
                    std::task::Poll::Ready(())
                } else {
                    reader.register_waker(cx.waker());
                    std::task::Poll::Pending
                }
            })
            .fuse();
            let tick = compio::time::sleep(STREAM_FLUSH_INTERVAL).fuse();
            pin_mut!(wait, tick);
            futures::select! {
                _ = wait => {}
                _ = tick => {
                    // Interval elapsed with no new data → record the duration
                    // delta so a long idle stream bills continuously.
                    flush_delta!();
                }
            }
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
    use zeroship_plugin_storage::StorageBackendConfig;
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

    // Regression: the `unlimited`/`enterprise` plan reports `wall_timeout =
    // None`, and `wall_limit` must pass that `None` straight through (no cap) so
    // a long single-request streaming upload isn't cut. Pre-fix this
    // `unwrap_or(30s)`'d the `None` — capping every unlimited request at 30 s,
    // which made >4 GiB `env.storage` streaming uploads time out at 30.37 s.
    #[test]
    fn wall_limit_passes_through_unlimited_none() {
        use zeroship_runtime::runtime::{Runtime, RuntimeLimits};
        init_runtime();

        // Unlimited plan → wall_timeout None → wall_limit None (NOT Some(30s)).
        let unlimited = Runtime::builder()
            .limits(RuntimeLimits {
                cpu_limit: None,
                wall_timeout: None,
                heap_limit_bytes: None,
            })
            .build();
        assert_eq!(
            wall_limit(&unlimited),
            None,
            "unlimited plan must have no dispatch wall cap"
        );

        // Bounded plan → its configured deadline is preserved unchanged.
        let bounded = Runtime::builder()
            .limits(RuntimeLimits {
                cpu_limit: None,
                wall_timeout: Some(std::time::Duration::from_secs(5)),
                heap_limit_bytes: None,
            })
            .build();
        assert_eq!(
            wall_limit(&bounded),
            Some(std::time::Duration::from_secs(5)),
            "bounded plan keeps its configured wall cap"
        );
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
                    storage_backend: None,
                    meter: std::sync::Arc::new(zeroship_metering::Meter::new()),
                },
            );
            assert!(crate::cache::load_app(
                app_id,
                source,
                AppRuntimeLimits::default(),
                None
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
                storage_backend: None,
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

    /// PR2-completion faithful regression: a real `/dispatch` request must
    /// feed ALL FIVE platform auto-counters into the per-app `Meter`, not
    /// just `requests`. This drives the REAL worker dispatch pipeline
    /// (ntex `/dispatch/{app_id}` → `record_request` → `call_fetch_handler`)
    /// — no shim — and then drains the very `Meter` the handler wrote to,
    /// asserting `cpu_us`, `wall_us`, `egress_bytes`, and `ingress_bytes`
    /// all landed alongside `requests`.
    ///
    /// Pre-fix this FAILS: `cache::record_request` only bumped `requests`,
    /// so cpu/wall/egress/ingress drain as zero.
    #[test]
    fn dispatch_feeds_all_five_platform_counters() {
        let Ok(runtime) = compio::runtime::Runtime::new() else {
            eprintln!("skipping (cannot create compio runtime)");
            return;
        };

        runtime.block_on(async {
            init_runtime();

            let app_id = Uuid::new_v4();
            // The handler echoes its request body so egress is deterministic,
            // and burns a little CPU in a loop so cpu_us is reliably > 0.
            let source = br#"
                export default {
                  fetch(req, env, ctx) {
                    let acc = 0;
                    for (let i = 0; i < 200000; i++) { acc += i % 7; }
                    return new Response("echo:" + acc.toString().slice(0, 0) + req.url);
                  }
                }
            "#;

            // Hold our own Arc<Meter> clone so we can drain what the handler
            // (which writes via the METER thread-local) recorded.
            let meter = std::sync::Arc::new(zeroship_metering::Meter::new());
            crate::cache::init_cache(
                10,
                crate::cache::KernelConfig {
                    db_url: None,
                    kv_url: None,
                    storage_backend: None,
                    meter: meter.clone(),
                },
            );
            assert!(crate::cache::load_app(
                app_id,
                source,
                AppRuntimeLimits::default(),
                None
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
            let blob_root = tmpdir("blob-meter");
            let blob_store: Arc<dyn BlobStore> =
                Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
            let config = Arc::new(crate::WorkerConfig {
                control_url: "http://127.0.0.1:1".to_string(),
                control_key: String::new(),
                db_url: None,
                kv_url: None,
                storage_backend: None,
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
                    .service(web::resource("/dispatch/{app_id}").route(web::post().to(dispatch))),
            )
            .await;

            // A non-empty request body → ingress_bytes must equal its length.
            let req_body = "the-end-user-request-body-payload";
            let url = "http://example.test/counters-probe";
            let envelope = serde_json::json!({
                "method": "POST",
                "url": url,
                "headers": [],
                "body": req_body,
            });
            let req = test::TestRequest::post()
                .uri(&format!("/dispatch/{app_id}"))
                .set_payload(serde_json::to_vec(&envelope).unwrap())
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            let resp_body_len = body.len() as u64;
            assert!(resp_body_len > 0, "handler returned a non-empty body");

            // Drain the SAME meter the handler fed — the faithful assertion.
            let snap = meter.drain();
            let usage = snap
                .get(&app_id)
                .expect("meter recorded usage for the dispatched app");

            assert_eq!(usage.requests, 1, "requests counter unchanged");
            assert_eq!(
                usage.ingress_bytes,
                req_body.len() as u64,
                "ingress_bytes must equal the request body length"
            );
            assert_eq!(
                usage.egress_bytes, resp_body_len,
                "egress_bytes must equal the response body length"
            );
            assert!(
                usage.wall_us > 0,
                "wall_us must be a positive elapsed-time measurement"
            );
            assert!(
                usage.cpu_us > 0,
                "cpu_us must be a positive CPU-time measurement"
            );

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
                    storage_backend: Some(StorageBackendConfig::Local(storage_root.clone())),
                    meter: std::sync::Arc::new(zeroship_metering::Meter::new()),
                },
            );
            assert!(crate::cache::load_app(
                app_id,
                source,
                AppRuntimeLimits::default(),
                None
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
                storage_backend: None,
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

    // -----------------------------------------------------------------------
    // Metering coverage (#27, H1) — SSE/streaming incremental flush
    // -----------------------------------------------------------------------

    /// Wire the cache's `METER` thread-local to a fresh meter we hold, so the
    /// drain task's `record_stream_delta` writes land somewhere we can drain.
    fn init_meter_for_stream_test() -> Arc<zeroship_metering::Meter> {
        let meter = Arc::new(zeroship_metering::Meter::new());
        crate::cache::init_cache(
            4,
            crate::cache::KernelConfig {
                db_url: None,
                kv_url: None,
                storage_backend: None,
                meter: Arc::clone(&meter),
            },
        );
        meter
    }

    /// Yield to the compio runtime enough times that the spawned stream-drain
    /// task gets scheduled and processes the chunks we pushed.
    async fn let_drain_run() {
        for _ in 0..8 {
            let _ = compio::runtime::spawn(async {}).await;
        }
    }

    /// THE H1 regression: a long stream accrues `egress_bytes` +
    /// `stream_wall_us` deltas INCREMENTALLY — usage is recorded BEFORE the
    /// stream finalizes (so a multi-hour stream bills continuously and a
    /// crash loses ≤ one interval), not only once at close.
    ///
    /// Drives the REAL `stream_response` drain (the worker's streaming path)
    /// against a real `StreamReader`, pushing > STREAM_FLUSH_BYTES so the
    /// byte-threshold mid-stream flush fires, then drains the meter while the
    /// stream is STILL OPEN and asserts a non-zero partial. RED pre-fix: the
    /// old drain recorded nothing until `on_complete` at finalize.
    #[test]
    fn long_stream_accrues_egress_before_close() {
        let Ok(rt) = compio::runtime::Runtime::new() else {
            eprintln!("skipping (cannot create compio runtime)");
            return;
        };
        rt.block_on(async {
            let meter = init_meter_for_stream_test();
            let app_id = Uuid::new_v4();

            // Generous cap so a > 1 MiB push isn't rejected by the per-stream
            // overflow guard.
            let (writer, reader) =
                zeroship_runtime::core::channel::stream_buffer_with_cap(16 * 1024 * 1024);

            // Hold the response so its `rx` stays alive (a dropped rx would
            // make `tx.send` fail and finalize early).
            let _resp = stream_response(200, &[], reader, app_id);

            // Push > STREAM_FLUSH_BYTES so the mid-stream byte-threshold flush
            // fires while the stream is still open (NOT closed yet).
            let big = vec![b'x'; (STREAM_FLUSH_BYTES + 4096) as usize];
            let pushed = big.len() as u64;
            assert!(matches!(
                writer.push(big),
                zeroship_runtime::core::channel::StreamPushResult::Ok
            ));

            let_drain_run().await;

            // BEFORE close: a delta must already be recorded (the crash-loss
            // bound). Pre-fix this is empty (finalize-only recording).
            let snap = meter.drain();
            let usage = snap
                .get(&app_id)
                .expect("a streaming delta must be recorded BEFORE the stream closes");
            assert_eq!(
                usage.egress_bytes, pushed,
                "the mid-stream byte-threshold flush records the streamed bytes \
                 before finalize"
            );
            assert!(
                usage.custom.get("stream_wall_us").copied().unwrap_or(0) >= 0,
                "stream_wall_us is recorded as a custom metric on the incremental flush"
            );
            // The drain task NEVER counts `requests` (a stream is one request,
            // counted by record_stream_unary — not exercised here).
            assert_eq!(usage.requests, 0, "stream_response must not touch requests");

            // Push a second batch, then close → the final delta lands the
            // remainder. The total across deltas equals the bytes streamed.
            let more = vec![b'y'; 2048];
            let more_len = more.len() as u64;
            assert!(matches!(
                writer.push(more),
                zeroship_runtime::core::channel::StreamPushResult::Ok
            ));
            writer.close();
            let_drain_run().await;

            let snap2 = meter.drain();
            // `drain()` reset after the first read, so this second drain holds
            // only the post-first-drain deltas (the second batch + any final
            // wall delta).
            let usage2 = snap2.get(&app_id).expect("final delta recorded at close");
            assert_eq!(
                usage2.egress_bytes, more_len,
                "the final delta records the remaining streamed bytes"
            );
            assert_eq!(usage2.requests, 0, "still no requests from the drain");
        });
    }

    /// `requests` is counted EXACTLY ONCE for a stream (a stream is one
    /// request), by `record_stream_unary` at stream start — never by the
    /// per-delta drain. This pins that the unary recorder bumps `requests`
    /// once and the drain bumps it zero times, so N incremental deltas can
    /// never inflate the request count.
    #[test]
    fn streaming_counts_request_exactly_once() {
        let Ok(rt) = compio::runtime::Runtime::new() else {
            eprintln!("skipping (cannot create compio runtime)");
            return;
        };
        rt.block_on(async {
            let meter = init_meter_for_stream_test();
            let app_id = Uuid::new_v4();

            // The unary recorder runs once at stream start (as the dispatch
            // arm does), counting requests + cpu + ingress.
            let wall_start = std::time::Instant::now();
            record_stream_unary(app_id, 123, 456, wall_start);

            let (writer, reader) =
                zeroship_runtime::core::channel::stream_buffer_with_cap(16 * 1024 * 1024);
            let _resp = stream_response(200, &[], reader, app_id);

            // Two large batches → two mid-stream byte-threshold deltas + a
            // final delta = three drain-side increments of egress/stream_wall.
            for _ in 0..2 {
                let big = vec![b'z'; (STREAM_FLUSH_BYTES + 1) as usize];
                assert!(matches!(
                    writer.push(big),
                    zeroship_runtime::core::channel::StreamPushResult::Ok
                ));
                let_drain_run().await;
            }
            writer.close();
            let_drain_run().await;

            let snap = meter.drain();
            let usage = snap.get(&app_id).expect("usage recorded for the stream");
            assert_eq!(
                usage.requests, 1,
                "a stream counts exactly one request despite many incremental deltas"
            );
            assert_eq!(usage.cpu_us, 123, "unary cpu_us recorded once");
            assert_eq!(usage.ingress_bytes, 456, "unary ingress_bytes recorded once");
            assert!(usage.egress_bytes > 0, "incremental egress accrued across deltas");
            assert!(
                usage.custom.get("stream_wall_us").copied().unwrap_or(0) > 0
                    || usage.egress_bytes > 0,
                "stream_wall_us accrues over the stream lifetime"
            );
        });
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
    if !cache::load_app(
        *app_id,
        &bytes,
        app_version.runtime.clone(),
        app_version.deploy_hash.as_deref(),
    ) {
        crate::sync::remove_env(envs, app_id);
        return Err("failed to parse bundle".into());
    }
    // Record what this isolate was loaded against so the reconcile loop
    // can detect future swaps: the deploy_hash (the canonical manifest
    // hash, NOT the per-blob bundle hash) and the env version the env we
    // just committed was fetched at. The env half matters for SEC-7 —
    // without it, a later env-only rotation would be invisible to
    // `sync::needs_reload` and the isolate would keep serving revoked
    // credentials.
    cache::set_loaded_meta(*app_id, cache::LoadedMeta {
        deploy_hash: app_version.deploy_hash.clone(),
        env_version: app_version.env_version,
    });
    tracing::info!(
        app_id = %app_id,
        blob_prefix = &bundle_hash[..bundle_hash.len().min(8)],
        "worker: on-demand loaded app"
    );
    Ok(())
}
