use std::sync::Arc;

use futures::{pin_mut, FutureExt};
use ntex::http::body::{BodySize, MessageBody};
use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use uuid::Uuid;

use zeroship_core::app_id::AppId;
use zeroship_core::dispatch_frame::decode_dispatch_frame;
use zeroship_core::service_identity::{endpoints, ServiceEndpoint};
use zeroship_core::service_peers::ServiceAuth;
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
/// granularity), distinct from the meter's stream-outbox cadence. The interval
/// matches the outbox cadence so a recorded delta is rarely stranded in-memory
/// more than one drain.
const STREAM_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
/// ~1 MiB bounds the in-memory un-recorded egress between deltas.
const STREAM_FLUSH_BYTES: u64 = 1024 * 1024;

/// Cap on the decoded creator-app request body.
///
/// Shared with the gateway so both tiers admit the same request. Enforcing it
/// here is not enough on its own: the check below only runs once the `Bytes`
/// extractor has accepted the frame, so the route must also carry a
/// `PayloadConfig` of [`zeroship_core::dispatch_frame::MAX_DISPATCH_FRAME_BYTES`].
/// Without one, ntex applies its own 256 KiB default and rejects anything
/// larger with a bare 400 before this handler is ever entered.
pub const MAX_DISPATCH_BODY_BYTES: usize = zeroship_core::dispatch_frame::MAX_REQUEST_BODY_BYTES;

/// Verify the CALLER's own service credential on the dispatch endpoints.
///
/// The TRANSPORT-ONLY assertion profile: an ed25519 JWT the gateway signs with
/// a key only the gateway holds, verified here under the gateway's published
/// public half, with no `jti` claimed and no shared store consulted. This hop
/// carries every end-user request, so a single-use claim would put a write
/// against a table shared by every worker replica on the app data path; the
/// identity envelope's binding to the dispatch request id and its issuance
/// window is what bounds replay here instead.
///
/// It answers ONE question - which service is calling - and deliberately says
/// nothing about the end user. That is [`verified_user_json`]'s job, and the
/// two are now separate credentials under separate keys, which is what makes
/// the guarantee on `encode_user_header` writable at all.
///
/// # There is no bypass
///
/// An unconfigured [`ServiceAuth`] REFUSES. Absence of key material is a
/// closed door, not an open one.
pub(crate) async fn check_worker_auth(
    req: &HttpRequest,
    service_auth: &ServiceAuth,
    endpoint: ServiceEndpoint,
) -> Option<HttpResponse> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());
    match service_auth.verify(header, endpoint).await {
        Ok(_identity) => None,
        Err(error) => {
            tracing::warn!(%error, path = %req.path(), "worker: caller rejected");
            metrics::inc(&metrics::DISPATCH_REJECTED_AUTH);
            Some(HttpResponse::Unauthorized().body(r#"{"error":"unauthorized"}"#))
        }
    }
}

/// Verify the END-USER identity the gateway forwarded, under the GATEWAY's
/// public key.
///
/// A second, independent credential from the one [`check_worker_auth`] checks -
/// independent now in the only sense that counts, which is the key. Until this
/// change both were the shared `worker_key`, so a worker able to verify an
/// envelope was equally able to mint one, and every claim about the envelope
/// surviving a transport bypass was circular. The worker now holds only the
/// public half: it can check the gateway's signature and cannot produce one.
///
/// # Absence refuses
///
/// No verifier - a worker started with no service key material, or one whose
/// peer document named no gateway key - REFUSES a request that carries an
/// envelope, rather than passing the identity through unchecked or dropping it
/// to anonymous. In practice such a worker never gets here, because
/// [`check_worker_auth`] has already refused the caller; the arm exists so the
/// answer does not depend on the order of two guards.
fn verified_user_json(
    req: &HttpRequest,
    service_auth: &ServiceAuth,
) -> Result<Option<String>, HttpResponse> {
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
    let Some(verifier) = service_auth.user_envelope_verifier() else {
        tracing::error!(
            "worker: an identity envelope arrived but no gateway public key is configured"
        );
        metrics::inc(&metrics::DISPATCH_REJECTED_AUTH);
        return Err(HttpResponse::Unauthorized().body(r#"{"error":"invalid user header"}"#));
    };
    match verifier.verify_for_request(header, expected_request_id) {
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
/// (`wall_timeout` unset) ⇒ no cap. Bounded plans keep their configured
/// `Some(_)` deadline.
fn wall_limit(runtime: &Runtime) -> Option<std::time::Duration> {
    runtime.wall_timeout()
}

// ---------------------------------------------------------------------------
// Unified dispatch — the worker's single entry point
// ---------------------------------------------------------------------------

/// Registers the dispatch surface, payload limits included.
///
/// The server and the tests both go through here deliberately. These caps are
/// enforced by the route's `PayloadConfig`, not by the handler body, so a test
/// that wires the route itself would be measuring a limit the server does not
/// have.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/dispatch/{app_id}")
            .state(web::types::PayloadConfig::new(
                zeroship_core::dispatch_frame::MAX_DISPATCH_FRAME_BYTES,
            ))
            .route(web::post().to(dispatch)),
    );
}

/// Dispatch an HTTP request through the V8 fetch handler.
///
/// The gateway forwards a dispatch frame (method, URL, headers, raw body
/// bytes) and the worker hands it to `Runtime::call_fetch_handler`, which
/// invokes the app's exported `default.fetch(req, env, ctx)`. Response may be
/// buffered or streaming (SSE); WebSocket upgrades aren't reachable through
/// this endpoint (the gateway uses a separate WS proxy path).
///
/// The length-prefixed dispatch frame the gateway sends carries the full HTTP
/// request shape (method, URL, headers, raw body bytes), and
/// `Runtime::call_fetch_handler` dispatches it through the kernel's
/// three-tier path:
///   1. `default.rpc(name, input, ctx)` for `/__zeroship/v1/<id>` URLs.
///   2. `default.fetchFast(method, url, bodyBytes, env)` for non-RPC traffic.
///   3. `default.fetch(request, env, ctx)` (WinterCG slow path) for
///      everything else, including fall-through from (1) and (2).
pub async fn dispatch(
    req: HttpRequest,
    config: web::types::State<Arc<WorkerConfig>>,
    envs: web::types::State<SharedEnvs>,
    logs: web::types::State<crate::logs::SharedLogs>,
    path: web::types::Path<String>,
    body: Bytes,
) -> HttpResponse {
    // Authenticate the gateway before touching the runtime.
    if let Some(resp) =
        check_worker_auth(&req, &config.service_auth, endpoints::WORKER_DISPATCH).await
    {
        return resp;
    }
    let user_json = match verified_user_json(&req, &config.service_auth) {
        Ok(user_json) => user_json,
        Err(resp) => return resp,
    };

    // Wall clock for the reject arms below. Dispatch keeps its own, started
    // after on-demand load, so a cold start is not billed as app wall time.
    let handler_start = std::time::Instant::now();

    let app_id = match AppId::parse(path.as_str()) {
        Ok(id) => id,
        Err(_) => {
            // The only reject with no app to attribute to: the id did not
            // parse, so there is no subject to meter against.
            metrics::inc(&metrics::DISPATCH_REJECTED_BAD_APP_ID);
            return HttpResponse::BadRequest().body(r#"{"error":"invalid app_id"}"#);
        }
    };

    // Rejects that happen before dispatch still consumed platform work: the
    // worker read the bytes and produced a response. Metering them keeps a
    // rejected request from being a free channel, and matches the env-missing
    // arm further down, which has always recorded. CPU is zero because no
    // isolate ran.
    let record_reject = |response: &HttpResponse, ingress_bytes: u64| {
        cache::record_request(
            &app_id,
            0,
            handler_start.elapsed().as_micros() as u64,
            buffered_response_body_len(response),
            ingress_bytes,
        );
    };

    // Parse the metadata prefix from the dispatch frame. The remaining bytes
    // are the creator-app request body; keep them as a refcounted `Bytes`
    // slice so binary uploads are byte-exact and not copied here.
    let (metadata, request_body) = match decode_dispatch_frame(body.as_ref()) {
        Ok(parts) => {
            let request_body = body.slice(parts.body_offset..);
            (parts.metadata, request_body)
        }
        Err(e) => {
            metrics::inc(&metrics::DISPATCH_REJECTED_BAD_ENVELOPE);
            let response = HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": format!("invalid envelope: {e}")}));
            // The frame did not decode, so the inner body length is unknowable;
            // the raw bytes received off the wire are what we actually read.
            record_reject(&response, body.len() as u64);
            return response;
        }
    };
    if request_body.len() > MAX_DISPATCH_BODY_BYTES {
        metrics::inc(&metrics::DISPATCH_REJECTED_BODY_TOO_LARGE);
        let response = HttpResponse::PayloadTooLarge()
            .json(&serde_json::json!({"error": "dispatch body too large"}));
        record_reject(&response, request_body.len() as u64);
        return response;
    }

    // On-demand loading: if app is not cached, pull from control plane.
    if cache::get_runtime(&app_id).is_none() {
        metrics::inc(&metrics::ON_DEMAND_LOADS_TOTAL);
        if let Err(e) = load_on_demand(&config, &envs, &app_id).await {
            metrics::inc(&metrics::ON_DEMAND_LOAD_FAILURES);
            let response = HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": format!("failed to load app: {e}")}));
            record_reject(&response, request_body.len() as u64);
            return response;
        }
    }

    // The DECLARED route policy, enforced in Rust before anything reaches V8.
    //
    // Placed HERE deliberately, and the position is the point:
    //
    //   * AFTER the on-demand load, because the policy arrives with the deploy
    //     - there is nothing to consult until the app is resident.
    //   * BEFORE `get_runtime` and the isolate lease, so a refused request
    //     neither stamps the app's LRU recency nor holds an isolate.
    //   * BEFORE `call_fetch_handler_with_user`, which is the whole reason it
    //     exists: `env.auth.requireUser()` runs INSIDE creator code and only
    //     when the creator remembers to call it, so it cannot be the fence.
    //
    // `user_json` here is the HMAC-verified `ZeroShip-User` payload resolved at
    // the top of this function, or `None` when the request carried no verified
    // identity at all. The scope half duplicates a check the gateway already
    // makes; that is defence in depth, not redundancy - see `policy.rs`, which
    // states the threat the duplication answers.
    if let Some(declared) = cache::get_declared_policy(&app_id) {
        if let Err(refusal) = crate::policy::enforce(&declared, &metadata.url, user_json.as_deref())
        {
            refusal.log(&app_id, &metadata.method, &metadata.url);
            let response = refusal.response();
            record_reject(&response, request_body.len() as u64);
            return response;
        }
    }

    let runtime = match cache::get_runtime(&app_id) {
        Some(r) => r,
        None => {
            let response = HttpResponse::NotFound().body(format!(
                r#"{{"error":"app {} not loaded"}}"#,
                app_id.as_str()
            ));
            record_reject(&response, request_body.len() as u64);
            return response;
        }
    };

    // Pin the isolate for the whole dispatch.
    //
    // The isolate cache is thread-local and this executor is single-threaded:
    // every `.await` below (the pending-promise channel, the wall-clock
    // timeout, `env.db`/fetch round-trips inside user code) hands the thread to
    // another dispatch. If that dispatch loads a different app while the cache
    // is at `max_size`, `cache::evict_lru` picks a victim by recency - and the
    // isolate we are in the middle of driving is a legal candidate. Eviction is
    // destructive: it closes the isolate's native sockets and fires every
    // in-flight `AbortController` before dropping the entry.
    //
    // The lease is the guard both eviction paths already filter on
    // (`cache.rs`, `!entry.runtime.is_isolate_leased()`). Holding it here is
    // what makes those filters reachable in production.
    //
    // RAII, deliberately: the guard is bound to a local, so every exit from
    // this function - `return`, `?`, unwind - releases it. There is no explicit
    // release call to forget on a new early-return path. It is also taken
    // AFTER `get_runtime` returned, i.e. after that function's borrow of the
    // `CACHE` thread-local has ended, so leasing cannot re-enter the borrow.
    //
    // Consequence, by design: when the cache is full and every isolate is
    // leased, `evict_lru` now returns false and the competing load is refused
    // with "isolate cache full and every isolate is leased; load deferred"
    // rather than corrupting a running dispatch.
    let _isolate_lease = runtime.lease_isolate();

    metrics::inc(&metrics::DISPATCH_TOTAL);

    // Metering auto-counters: start the wall clock now so it spans the whole
    // dispatch (V8 entry + any pending-promise await). The full five-counter
    // record happens once at the end of dispatch, when egress is known — see
    // `cache::record_request` below.
    let wall_start = std::time::Instant::now();

    // ingress_bytes = the end-user request body bytes the worker received
    // (the inner envelope body, not the JSON envelope wrapper overhead).
    let ingress_bytes = request_body.len() as u64;

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
            let response = HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": "env unavailable"}));
            cache::record_request(
                &app_id,
                0,
                wall_start.elapsed().as_micros() as u64,
                buffered_response_body_len(&response),
                ingress_bytes,
            );
            return response;
        }
    };
    let cancel = CancelFlag::new();
    let ctx = RequestCtx::new(cancel.clone());

    // Enter isolate, dispatch through the unified fetch handler. Sample the
    // V8 thread's CPU clock (CLOCK_THREAD_CPUTIME_ID — the same clock the
    // CPU limiter arms) around the synchronous isolate entry: the delta is
    // the real CPU time this request burned in V8.
    //
    // This is the REQUEST-attributable half of the app's CPU, not all of it.
    // For a `Pending` handler the async continuation runs on the shared V8
    // actor thread via the pump, and no per-request accumulator the kernel
    // exposes could attribute it back to this request. That does not make it
    // unbillable: `RuntimeInner::bill_pump_cpu` samples the same CPU clock
    // around every pump V8 window and emits it to the app's `cpu_us` meter,
    // which is keyed by app — the granularity billing consumes. So the
    // number recorded here is a partial figure for THIS request and a
    // complete one for nothing; the app's `cpu_us` total is this plus the
    // pump's contribution.
    let cpu_start = zeroship_runtime::init::thread_cpu_time();
    let outcome = {
        runtime.enter_isolate();
        let o = runtime.call_fetch_handler_with_user(
            &metadata.method,
            &metadata.url,
            &metadata.headers,
            request_body.as_ref(),
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
        FetchOutcome::Response {
            status,
            headers,
            body,
            logs: request_logs,
        } => {
            crate::logs::append(&logs, &app_id, request_logs);
            record(body.len() as u64);
            make_http_response(status, headers, body)
        }
        FetchOutcome::Stream {
            status,
            headers,
            body_reader,
            logs: request_logs,
        } => {
            crate::logs::append(&logs, &app_id, request_logs);
            record_stream_unary(&app_id, cpu_us, ingress_bytes, wall_start);
            stream_response(status, &headers, body_reader, app_id)
        }
        FetchOutcome::WebSocketUpgrade { .. } => {
            // WS upgrades over the HTTP dispatch endpoint aren't supported —
            // the gateway uses a separate WS proxy path for websocket traffic.
            let response = make_error_msg(500, "WebSocket upgrade not supported via HTTP dispatch");
            record(buffered_response_body_len(&response));
            response
        }
        FetchOutcome::Pending { rx, cancel: cf } => {
            match recv_with_timeout(&rx, wall_limit(&runtime), &cf, &runtime).await {
                Some(Ok(SettledFetch::Response {
                    status,
                    headers,
                    body,
                    logs: request_logs,
                })) => {
                    crate::logs::append(&logs, &app_id, request_logs);
                    record(body.len() as u64);
                    make_http_response(status, headers, body)
                }
                Some(Ok(SettledFetch::Stream {
                    status,
                    headers,
                    body_reader,
                    logs: request_logs,
                })) => {
                    crate::logs::append(&logs, &app_id, request_logs);
                    record_stream_unary(&app_id, cpu_us, ingress_bytes, wall_start);
                    stream_response(status, &headers, body_reader, app_id)
                }
                Some(Ok(SettledFetch::WebSocketUpgrade {
                    logs: request_logs, ..
                })) => {
                    crate::logs::append(&logs, &app_id, request_logs);
                    let response =
                        make_error_msg(500, "WebSocket upgrade not supported via HTTP dispatch");
                    record(buffered_response_body_len(&response));
                    response
                }
                Some(Err(e)) => {
                    let response = make_error(&e);
                    record(buffered_response_body_len(&response));
                    response
                }
                None => {
                    let response = make_error_msg(504, "request timed out");
                    record(buffered_response_body_len(&response));
                    response
                }
            }
        }
    }
}
///
/// A stream is one request, so `requests`/`cpu_us`/`ingress_bytes` (and the
/// unary `wall_us` — the synchronous-handler elapsed up to stream start) are
/// recorded here a single time, immediately — not deferred to finalize where
/// a crash would lose them. `egress_bytes` is recorded as 0 here; the
/// streamed body bytes and the held-open duration (`stream_wall_us`) accrue
/// INCREMENTALLY in the drain task via [`cache::record_stream_delta`], so a
/// multi-hour stream bills continuously and a crash loses ≤ one interval.
fn record_stream_unary(
    app_id: &AppId,
    cpu_us: u64,
    ingress_bytes: u64,
    wall_start: std::time::Instant,
) {
    let wall_us = wall_start.elapsed().as_micros() as u64;
    // egress = 0: the streamed body accrues as incremental `egress_bytes`
    // deltas in the drain; `requests` is counted once here and never again.
    cache::record_request(app_id, cpu_us, wall_us, 0, ingress_bytes);
}

/// Build an HTTP response forwarding the JS handler's status, headers, and body.
///
/// Body is moved, not copied — for large responses (image uploads, large
/// JSON payloads) this halves the memory churn per request.
fn make_http_response(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> HttpResponse {
    let status_code =
        ntex::http::StatusCode::from_u16(status).unwrap_or(ntex::http::StatusCode::OK);
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
    app_id: AppId,
) -> HttpResponse {
    let status_code =
        ntex::http::StatusCode::from_u16(status).unwrap_or(ntex::http::StatusCode::OK);
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
    // one interval's delta, instead of the whole stream. `requests`/`cpu_us`/
    // `ingress_bytes` were already recorded once at stream start
    // (`record_stream_unary`) and are NOT touched here — a stream is one
    // request.
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
                let wall_delta =
                    elapsed_us.saturating_sub(std::mem::replace(&mut wall_recorded_us, elapsed_us));
                cache::record_stream_delta(
                    &app_id,
                    std::mem::take(&mut bytes_since_flush),
                    wall_delta,
                );
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
                    if tx
                        .send(Ok::<Bytes, std::io::Error>(Bytes::from(chunk)))
                        .is_err()
                    {
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
    })
    .detach();

    builder.streaming(rx)
}

fn buffered_response_body_len(response: &HttpResponse) -> u64 {
    match response.body().size() {
        BodySize::Sized(len) => len,
        BodySize::None | BodySize::Empty => 0,
        BodySize::Stream => unreachable!("buffered response has a streaming body"),
    }
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
/// against the OLD env on a later step in this function. Everything is
/// staged in locals first and cache is only mutated at the end.
///
/// The normal app manifest selects its complete module graph and descriptor.
async fn load_on_demand(
    config: &WorkerConfig,
    envs: &SharedEnvs,
    app_id: &AppId,
) -> Result<(), String> {
    let app_version =
        crate::sync::fetch_app_version(&config.control_url, &config.service_auth, app_id).await?;

    let manifest = app_version
        .manifest
        .as_ref()
        .ok_or_else(|| format!("app {} has no manifest yet", app_id.as_str()))?;
    let executable = crate::executable::load_executable(manifest, &config.blob_store).await?;

    // Fetch env BEFORE committing the V8 isolate. If env fetch fails
    // we never partially-load.
    let env_json = crate::sync::fetch_app_env(&config.control_url, &config.service_auth, app_id)
        .await
        .map_err(|e| format!("env fetch failed: {e}"))?;

    // Now commit both atomically (env first so dispatchers always see
    // env present once runtime is present).
    if let Err(e) =
        crate::sync::put_env_from_json(envs, app_id.clone(), &env_json, app_version.env_version)
    {
        return Err(format!("env parse failed: {e}"));
    }
    let env_entry = crate::sync::get_env(envs, app_id)
        .ok_or_else(|| "env cache missing after env insert".to_string())?;
    cache::load_app(
        app_id.clone(),
        executable.modules,
        app_version.runtime.clone(),
        app_version.net_policy.clone(),
        app_version.deploy_hash.as_deref(),
        executable.descriptor.as_deref(),
        manifest,
        &env_entry.snapshot,
    )
    .await
    .map_err(|e| {
        crate::sync::remove_env(envs, app_id);
        format!("failed to load bundle: {e}")
    })?;
    // Record what this isolate was loaded against so the reconcile loop
    // can detect future swaps: the deploy_hash (the canonical manifest
    // hash, NOT the per-blob bundle hash) and the env version the env we
    // just committed was fetched at. The env half matters for SEC-7 —
    // without it, a later env-only rotation would be invisible to
    // `sync::needs_reload` and the isolate would keep serving revoked
    // credentials.
    cache::set_loaded_meta(
        app_id.clone(),
        cache::LoadedMeta {
            deploy_hash: app_version.deploy_hash.clone(),
            env_version: app_version.env_version,
            net_policy: app_version.net_policy,
        },
    );
    tracing::info!(
        app_id = app_id.as_str(),
        deploy_hash = ?app_version.deploy_hash,
        "worker: on-demand loaded app"
    );
    Ok(())
}
#[cfg(test)]
mod tests;

