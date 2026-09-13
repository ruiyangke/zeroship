use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::{pin_mut, FutureExt};
use ntex::http::body::{BodySize, MessageBody};
use ntex::web::{self, HttpRequest, HttpResponse};
use ntex::util::Bytes;
use serde_json::Value;
use uuid::Uuid;

use zeroship_core::app_id::AppId;
use zeroship_core::service_identity::{endpoints, ServiceEndpoint};
use zeroship_core::service_peers::ServiceAuth;
use zeroship_core::dispatch_frame::decode_dispatch_frame;
use zeroship_core::types::{AppNetPolicy, AppRuntimeLimits};
use zeroship_bundle::sha256_hex;
use zeroship_workflow::advance::{
    collect_post_apply_registrations, worker_json_to_step_result, WorkflowAdvanceNackKind,
    WorkflowAdvanceResponse, WorkflowRunDispatchRequest,
};
use zeroship_workflow::apply;
use zeroship_workflow::claim::{
    claim_workflow_run, renew_workflow_claim, WorkflowClaimOutcome,
};
use zeroship_workflow::engine::{StepRequest, WorkflowEngineConfig};
use zeroship_workflow::errors::WorkflowError;
use zeroship_workflow::store::pg::PgStore;
use zeroship_runtime::runtime::DispatchError;
use zeroship_runtime::{
    CancelFlag, EnvSnapshot, FetchOutcome, RequestCtx, ResultReceiver, Runtime, SettledFetch,
    SettledWorkflow, StreamReader, WorkflowOutcome,
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

const WORKFLOW_INLINE_OUTPUT_CAP_BYTES: usize = 1024 * 1024;
static PROVISIONED_WORKFLOW_JOURNALS: OnceLock<Mutex<HashSet<AppId>>> = OnceLock::new();

/// Cap on the decoded creator-app request body.
///
/// Shared with the gateway so both tiers admit the same request. Enforcing it
/// here is not enough on its own: the check below only runs once the `Bytes`
/// extractor has accepted the frame, so the route must also carry a
/// `PayloadConfig` of [`zeroship_core::dispatch_frame::MAX_DISPATCH_FRAME_BYTES`].
/// Without one, ntex applies its own 256 KiB default and rejects anything
/// larger with a bare 400 before this handler is ever entered - which is what
/// it did until this was wired up.
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
/// The predecessor returned `None` - authorized - when the shared secret was
/// empty, so an unconfigured worker accepted every caller. An unconfigured
/// [`ServiceAuth`] REFUSES instead. Absence of key material is now a closed
/// door, not an open one.
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

/// Registers the dispatch surface, payload limits included.
///
/// The server and the tests both go through here deliberately. These caps are
/// enforced by the route's `PayloadConfig`, not by the handler body, so a test
/// that wires the route itself would be measuring a limit the server does not
/// have. That is not hypothetical: the size check inside [`dispatch`] was
/// unreachable in production for as long as this registration lived only in
/// `main.rs`, and no test could tell, because every test wired its own route.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/dispatch/{app_id}")
            .state(web::types::PayloadConfig::new(
                zeroship_core::dispatch_frame::MAX_DISPATCH_FRAME_BYTES,
            ))
            .route(web::post().to(dispatch)),
    )
    .service(
        web::resource("/workflow-advance-unsigned/{app_id}")
            .state(web::types::PayloadConfig::new(MAX_DISPATCH_BODY_BYTES))
            .route(web::post().to(workflow_advance_unsigned)),
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
        FetchOutcome::Response { status, headers, body, logs: request_logs } => {
            crate::logs::append(&logs, &app_id, request_logs);
            record(body.len() as u64);
            make_http_response(status, headers, body)
        }
        FetchOutcome::Stream { status, headers, body_reader, logs: request_logs } => {
            crate::logs::append(&logs, &app_id, request_logs);
            record_stream_unary(&app_id, cpu_us, ingress_bytes, wall_start);
            stream_response(status, &headers, body_reader, app_id)
        }
        FetchOutcome::WebSocketUpgrade { .. } => {
            // WS upgrades over the HTTP dispatch endpoint aren't supported —
            // the gateway uses a separate WS proxy path for websocket traffic.
            let response =
                make_error_msg(500, "WebSocket upgrade not supported via HTTP dispatch");
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
                Some(Ok(SettledFetch::WebSocketUpgrade { logs: request_logs, .. })) => {
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

static WORKFLOW_WORKER_OWNER_ID: OnceLock<String> = OnceLock::new();

fn workflow_worker_owner_id() -> String {
    WORKFLOW_WORKER_OWNER_ID
        .get_or_init(|| format!("worker-wf-{}", std::process::id()))
        .clone()
}

fn workflow_worker_config() -> WorkflowEngineConfig {
    WorkflowEngineConfig {
        owner_id: workflow_worker_owner_id(),
        ..WorkflowEngineConfig::default()
    }
}

fn workflow_runtime_envelope(request: &StepRequest) -> serde_json::Value {
    serde_json::json!({
        "runId": &request.run_id,
        "workflowName": &request.workflow_name,
        "trigger": {
            "input": request.input.clone().unwrap_or(Value::Null),
            "startedAt": request.started_at.to_rfc3339(),
            "runId": &request.run_id,
            "workflowName": &request.workflow_name,
        },
        "journal": request.journal.clone(),
        "phase": &request.phase,
        "deployHash": &request.deploy_hash,
        "attempt": 0,
        "nonce": &request.dispatch_nonce,
        "ownerId": &request.owner_id,
        "stuckStrikeLimit": request.stuck_strike_limit,
        "maxChildDepth": request.max_child_depth,
        "maxLiveDescendants": request.max_live_descendants,
        "maxStartManyBatch": request.max_start_many_batch,
        "journalLimits": request.journal_limits,
    })
}

/// Durable-workflow replay ingress that performs NO signature or nonce
/// verification. Signed advance is the production transport; this exists so
/// the replay path could be exercised before the signing work landed.
///
/// NOT test-only, and an earlier version of this comment said it was. There is
/// no `cfg` gate: this function is compiled into the production worker and the
/// route is registered unconditionally (see the `workflow/advance-unsigned`
/// route above). What refuses it in production is a RUNTIME check, not its
/// absence - the handler returns 403 unless `workflow_advance_unsigned` is set,
/// and that flag is hidden, defaults to false, and has no environment binding,
/// so it takes an explicit CLI argument to turn on. `deploy/` passes it
/// nowhere; the native workflow acceptance fixtures enable it explicitly.
///
/// The distinction is the point: "production never enables this" is a statement
/// about how the binary is invoked, not something the build enforces. Read it
/// as a default that holds, not as an endpoint that is missing.
pub async fn workflow_advance_unsigned(
    req: HttpRequest,
    config: web::types::State<Arc<WorkerConfig>>,
    envs: web::types::State<SharedEnvs>,
    logs: web::types::State<crate::logs::SharedLogs>,
    path: web::types::Path<String>,
    body: Bytes,
) -> HttpResponse {
    if let Some(resp) = check_worker_auth(
        &req,
        &config.service_auth,
        endpoints::WORKER_WORKFLOW_ADVANCE,
    )
    .await
    {
        return resp;
    }
    if !config.workflow_advance_unsigned {
        return HttpResponse::Forbidden()
            .json(&serde_json::json!({"error": "workflow advance unsigned disabled"}));
    }

    // Same shape and the same reason as `dispatch` above: the path carries the
    // typed id, and it is carried - not decoded - into everything below.
    let app_id = match AppId::parse(path.as_str()) {
        Ok(id) => id,
        Err(_) => {
            metrics::inc(&metrics::DISPATCH_REJECTED_BAD_APP_ID);
            return HttpResponse::BadRequest().body(r#"{"error":"invalid app_id"}"#);
        }
    };
    if body.len() > MAX_DISPATCH_BODY_BYTES {
        metrics::inc(&metrics::DISPATCH_REJECTED_BODY_TOO_LARGE);
        return HttpResponse::PayloadTooLarge()
            .json(&serde_json::json!({"error": "dispatch body too large"}));
    }
    let envelope_json = match std::str::from_utf8(body.as_ref()) {
        Ok(s) => s,
        Err(e) => {
            metrics::inc(&metrics::DISPATCH_REJECTED_BAD_ENVELOPE);
            return HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": format!("invalid utf-8 envelope: {e}")}));
        }
    };
    let parsed: WorkflowRunDispatchRequest = match serde_json::from_str(envelope_json) {
        Ok(v) => v,
        Err(e) => {
            metrics::inc(&metrics::DISPATCH_REJECTED_BAD_ENVELOPE);
            return HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": format!("invalid workflow envelope: {e}")}));
        }
    };
    if parsed.run_id.is_empty() || parsed.app_id != app_id {
        metrics::inc(&metrics::DISPATCH_REJECTED_BAD_ENVELOPE);
        return HttpResponse::BadRequest().json(&serde_json::json!({
            "error": "workflow envelope requires matching appId and non-empty runId"
        }));
    }

    let Some(db_url) = cache::db_url() else {
        return HttpResponse::Ok().json(&WorkflowAdvanceResponse::nack(
            parsed.run_id.clone(),
            WorkflowAdvanceNackKind::Backpressure,
            "worker DB_URL is not configured",
        ));
    };
    if let Err(e) = ensure_workflow_journal_provisioned(&db_url, &app_id).await {
        return HttpResponse::Ok().json(&WorkflowAdvanceResponse::nack(
            parsed.run_id.clone(),
            WorkflowAdvanceNackKind::Backpressure,
            e,
        ));
    }

    let claim_config = workflow_worker_config();
    let claim = match claim_workflow_run(&db_url, &parsed, &claim_config).await {
        Ok(WorkflowClaimOutcome::Claimed(request)) => request,
        Ok(WorkflowClaimOutcome::Terminal(registrations)) => {
            return HttpResponse::Ok().json(&WorkflowAdvanceResponse::ack(
                parsed.run_id,
                registrations,
            ));
        }
        Ok(WorkflowClaimOutcome::ClaimLost) => {
            return HttpResponse::Ok().json(&WorkflowAdvanceResponse::nack(
                parsed.run_id,
                WorkflowAdvanceNackKind::ClaimLost,
                "workflow claim lost",
            ));
        }
        Ok(WorkflowClaimOutcome::Backpressure(reason)) => {
            return HttpResponse::Ok().json(&WorkflowAdvanceResponse::nack(
                parsed.run_id,
                WorkflowAdvanceNackKind::Backpressure,
                reason,
            ));
        }
        Err(e) => {
            return HttpResponse::Ok().json(&WorkflowAdvanceResponse::nack(
                parsed.run_id,
                WorkflowAdvanceNackKind::ApplyFailed,
                format!("claim workflow run: {e}"),
            ));
        }
    };

    if cache::get_workflow_runtime(&app_id, &claim.deploy_hash).is_none() {
        metrics::inc(&metrics::ON_DEMAND_LOADS_TOTAL);
        if let Err(e) =
            load_pinned_workflow_on_demand(&config, &envs, &app_id, &claim.deploy_hash).await
        {
            metrics::inc(&metrics::ON_DEMAND_LOAD_FAILURES);
            return HttpResponse::ServiceUnavailable().json(&serde_json::json!({
                "error": format!("failed to load pinned workflow app: {e}")
            }));
        }
    }

    let runtime = match cache::get_workflow_runtime(&app_id, &claim.deploy_hash) {
        Some(r) => r,
        None => {
            return HttpResponse::NotFound().json(&serde_json::json!({
                "error": format!(
                    "app {} deploy {} not loaded",
                    app_id.as_str(),
                    claim.deploy_hash
                )
            }));
        }
    };

    // Same pin as the fetch dispatch above, for the same reason: this function
    // drives user code (`call_workflow_dispatch` enters the isolate) and then
    // awaits - `recv_with_timeout` on the pending arm, plus the control-plane
    // round-trip in `apply_workflow_advance_result`. Pinned workflow isolates
    // live in `cache.workflow_isolates` and are evicted by
    // `evict_pinned_lru_for_app` when a second deploy hash for the SAME app
    // exceeds `max_pinned_isolates_per_app`; that path filters on the very
    // same lease. RAII local, taken after `get_workflow_runtime`'s `CACHE`
    // borrow has ended.
    let _isolate_lease = runtime.lease_isolate();

    let env: EnvSnapshot = match crate::sync::get_env(&envs, &app_id) {
        Some(entry) => entry.snapshot.clone(),
        None => {
            metrics::inc(&metrics::ENV_UNAVAILABLE_TOTAL);
            return HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": "env unavailable"}));
        }
    };

    let runtime_envelope = workflow_runtime_envelope(&claim);
    let runtime_envelope_json = match serde_json::to_string(&runtime_envelope) {
        Ok(json) => json,
        Err(e) => {
            return HttpResponse::Ok().json(&WorkflowAdvanceResponse::nack(
                claim.run_id.clone(),
                WorkflowAdvanceNackKind::Invalid,
                format!("encode workflow runtime envelope: {e}"),
            ));
        }
    };
    let heartbeat = spawn_workflow_heartbeat(
        db_url.clone(),
        claim.app_id.clone(),
        claim.run_id.clone(),
        claim.owner_id.clone(),
        claim.dispatch_nonce.clone(),
        claim_config.claim_ttl_ms,
        claim_config.heartbeat_ms,
    );

    metrics::inc(&metrics::DISPATCH_TOTAL);
    let ingress_bytes = body.len() as u64;
    let wall_start = std::time::Instant::now();
    let cancel = CancelFlag::new();
    let ctx = RequestCtx::new(cancel);
    let cpu_start = zeroship_runtime::init::thread_cpu_time();
    let outcome = {
        runtime.enter_isolate();
        let o = runtime.call_workflow_dispatch(&runtime_envelope_json, &env, ctx);
        runtime.exit_isolate();
        o
    };
    let cpu_us = zeroship_runtime::init::thread_cpu_time()
        .saturating_sub(cpu_start)
        .as_micros() as u64;

    let record = |egress_bytes: u64| {
        let wall_us = wall_start.elapsed().as_micros() as u64;
        cache::record_request(&app_id, cpu_us, wall_us, egress_bytes, ingress_bytes);
        cache::record_workflow_step(&app_id);
    };

    match outcome {
        WorkflowOutcome::Response { json, logs: request_logs } => {
            crate::logs::append(&logs, &app_id, request_logs);
            let response = match apply_workflow_advance_result(
                &config,
                &db_url,
                &claim,
                &heartbeat,
                json,
            )
            .await
            {
                Ok(response) => response,
                Err(resp) => {
                    record(0);
                    return resp;
                }
            };
            record(response.len() as u64);
            HttpResponse::Ok().content_type("application/json").body(response)
        }
        WorkflowOutcome::Pending { rx, cancel } => {
            match recv_with_timeout(&rx, wall_limit(&runtime), &cancel, &runtime).await {
                Some(Ok(SettledWorkflow { json, logs: request_logs })) => {
                    crate::logs::append(&logs, &app_id, request_logs);
                    let response = match apply_workflow_advance_result(
                        &config,
                        &db_url,
                        &claim,
                        &heartbeat,
                        json,
                    )
                    .await
                    {
                        Ok(response) => response,
                        Err(resp) => {
                            record(0);
                            return resp;
                        }
                    };
                    record(response.len() as u64);
                    HttpResponse::Ok().content_type("application/json").body(response)
                }
                Some(Err(e)) => {
                    record(0);
                    make_error(&e)
                }
                None => {
                    record(0);
                    make_error_msg(504, "workflow advance timed out")
                }
            }
        }
    }
}

struct WorkflowHeartbeat {
    active: Arc<AtomicBool>,
    lease_lost: Arc<AtomicBool>,
}

impl WorkflowHeartbeat {
    fn lease_lost(&self) -> bool {
        self.lease_lost.load(Ordering::SeqCst)
    }
}

impl Drop for WorkflowHeartbeat {
    fn drop(&mut self) {
        self.active.store(false, Ordering::SeqCst);
    }
}

fn spawn_workflow_heartbeat(
    db_url: String,
    app_id: AppId,
    run_id: String,
    owner_id: String,
    dispatch_nonce: String,
    claim_ttl_ms: i64,
    heartbeat_ms: u64,
) -> WorkflowHeartbeat {
    let active = Arc::new(AtomicBool::new(true));
    let lease_lost = Arc::new(AtomicBool::new(false));
    let heartbeat_active = Arc::clone(&active);
    let heartbeat_lost = Arc::clone(&lease_lost);
    let interval = Duration::from_millis(heartbeat_ms);
    compio::runtime::spawn(async move {
        while heartbeat_active.load(Ordering::SeqCst) {
            compio::time::sleep(interval).await;
            if !heartbeat_active.load(Ordering::SeqCst) {
                break;
            }
            match renew_workflow_claim(
                &db_url,
                &app_id,
                &run_id,
                &owner_id,
                &dispatch_nonce,
                claim_ttl_ms,
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => {
                    heartbeat_lost.store(true, Ordering::SeqCst);
                    heartbeat_active.store(false, Ordering::SeqCst);
                    tracing::warn!(run_id = %run_id, "worker workflow heartbeat lost claim");
                    break;
                }
                Err(e) => {
                    tracing::warn!(error = %e, run_id = %run_id, "worker workflow heartbeat failed");
                }
            }
        }
    })
    .detach();

    WorkflowHeartbeat { active, lease_lost }
}

async fn apply_workflow_advance_result(
    config: &WorkerConfig,
    db_url: &str,
    request: &StepRequest,
    heartbeat: &WorkflowHeartbeat,
    json: String,
) -> Result<String, HttpResponse> {
    if heartbeat.lease_lost() {
        let response = WorkflowAdvanceResponse::nack(
            request.run_id.clone(),
            WorkflowAdvanceNackKind::ApplyFailed,
            "workflow lease lost before apply",
        );
        return serde_json::to_string(&response).map_err(|e| {
            HttpResponse::ServiceUnavailable().json(&serde_json::json!({
                "error": format!("encode workflow advance ack: {e}")
            }))
        });
    }
    match renew_workflow_claim(
        db_url,
        &request.app_id,
        &request.run_id,
        &request.owner_id,
        &request.dispatch_nonce,
        WorkflowEngineConfig::default().claim_ttl_ms,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            let response = WorkflowAdvanceResponse::nack(
                request.run_id.clone(),
                WorkflowAdvanceNackKind::ApplyFailed,
                "workflow lease lost before apply",
            );
            return serde_json::to_string(&response).map_err(|e| {
                HttpResponse::ServiceUnavailable().json(&serde_json::json!({
                    "error": format!("encode workflow advance ack: {e}")
                }))
            });
        }
        Err(e) => {
            let response = WorkflowAdvanceResponse::nack(
                request.run_id.clone(),
                WorkflowAdvanceNackKind::Backpressure,
                format!("renew workflow claim before apply: {e}"),
            );
            return serde_json::to_string(&response).map_err(|e| {
                HttpResponse::ServiceUnavailable().json(&serde_json::json!({
                    "error": format!("encode workflow advance ack: {e}")
                }))
            });
        }
    }

    let json = rewrite_workflow_output_blobs(config, &request.app_id, json).await?;
    let response = match apply_workflow_advance_json(db_url, request, &json).await {
        Ok(response) | Err(response) => response,
    };
    serde_json::to_string(&response).map_err(|e| {
        HttpResponse::ServiceUnavailable().json(&serde_json::json!({
            "error": format!("encode workflow advance ack: {e}")
        }))
    })
}

async fn apply_workflow_advance_json(
    db_url: &str,
    request: &StepRequest,
    json: &str,
) -> Result<WorkflowAdvanceResponse, WorkflowAdvanceResponse> {
    let step_result = match worker_json_to_step_result(&request.run_id, &request.dispatch_nonce, json) {
        Ok(result) => result,
        Err(e) => {
            return Err(WorkflowAdvanceResponse::nack(
                request.run_id.clone(),
                WorkflowAdvanceNackKind::Invalid,
                format!("invalid workflow replay result: {e}"),
            ));
        }
    };

    if let Err(e) = ensure_workflow_journal_provisioned(db_url, &request.app_id).await {
        return Err(WorkflowAdvanceResponse::nack(
            request.run_id.clone(),
            WorkflowAdvanceNackKind::Backpressure,
            e,
        ));
    }

    let store = PgStore::new(db_url.to_string(), &request.app_id);
    let apply_config = workflow_apply_config_from_request(request);
    match apply::apply_step_result_on_store(&store, &apply_config, step_result).await {
        Ok(_applied) => match collect_post_apply_registrations(db_url, &request.app_id, &request.run_id, true).await {
            Ok(registrations) => Ok(WorkflowAdvanceResponse::ack(
                request.run_id.clone(),
                registrations,
            )),
            Err(e) => Err(WorkflowAdvanceResponse::nack(
                request.run_id.clone(),
                WorkflowAdvanceNackKind::ApplyFailed,
                format!("collect workflow advance registrations: {e}"),
            )),
        },
        Err(WorkflowError::Deadlock(e)) => Err(WorkflowAdvanceResponse::nack(
            request.run_id.clone(),
            WorkflowAdvanceNackKind::Deadlock,
            e,
        )),
        Err(WorkflowError::Invalid(e)) => Err(WorkflowAdvanceResponse::nack(
            request.run_id.clone(),
            WorkflowAdvanceNackKind::Invalid,
            e,
        )),
        Err(WorkflowError::CompensableCarry(e)) => Err(WorkflowAdvanceResponse::nack(
            request.run_id.clone(),
            WorkflowAdvanceNackKind::Invalid,
            format!("CompensableCarryError: {e}"),
        )),
        Err(WorkflowError::Db(e)) => Err(WorkflowAdvanceResponse::nack(
            request.run_id.clone(),
            WorkflowAdvanceNackKind::ApplyFailed,
            e,
        )),
    }
}

fn workflow_apply_config_from_request(request: &StepRequest) -> WorkflowEngineConfig {
    WorkflowEngineConfig {
        owner_id: request.owner_id.clone(),
        stuck_strike_limit: request.stuck_strike_limit,
        max_child_depth: request.max_child_depth,
        max_live_descendants: request.max_live_descendants,
        max_start_many_batch: request.max_start_many_batch,
        journal_limits: request.journal_limits,
        ..WorkflowEngineConfig::default()
    }
}

async fn ensure_workflow_journal_provisioned(db_url: &str, app_id: &AppId) -> Result<(), String> {
    let cache = PROVISIONED_WORKFLOW_JOURNALS.get_or_init(|| Mutex::new(HashSet::new()));
    if cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(app_id)
    {
        return Ok(());
    }

    let (client, connection) = compio_postgres::connect(db_url, compio_postgres::NoTls)
        .await
        .map_err(|e| format!("connect workflow journal db: {e}"))?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            tracing::error!(error = %e, "worker workflow provision pg connection error");
        }
    })
    .detach();
    PgStore::provision(&client, app_id)
        .await
        .map_err(|e| format!("provision workflow journal: {e}"))?;

    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(app_id.clone());
    Ok(())
}

async fn rewrite_workflow_output_blobs(
    config: &WorkerConfig,
    app_id: &AppId,
    json: String,
) -> Result<String, HttpResponse> {
    let mut value: Value = serde_json::from_str(&json).map_err(|e| {
        HttpResponse::ServiceUnavailable().json(&serde_json::json!({
            "error": format!("invalid workflow result json before spill rewrite: {e}")
        }))
    })?;

    if let Some(outcomes) = value.get_mut("outcomes").and_then(Value::as_array_mut) {
        for outcome in outcomes {
            rewrite_workflow_outcome_blob(config, app_id, outcome).await?;
        }
    } else {
        rewrite_workflow_outcome_blob(config, app_id, &mut value).await?;
    }

    serde_json::to_string(&value).map_err(|e| {
        HttpResponse::ServiceUnavailable().json(&serde_json::json!({
            "error": format!("encode workflow result after spill rewrite: {e}")
        }))
    })
}

async fn rewrite_workflow_outcome_blob(
    config: &WorkerConfig,
    app_id: &AppId,
    outcome: &mut Value,
) -> Result<(), HttpResponse> {
    let kind = outcome.get("kind").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "StepCompleted" => {
            let step_kind = outcome
                .get("stepKind")
                .and_then(Value::as_str)
                .unwrap_or("run");
            if step_kind != "run" {
                strip_output_mode_metadata(outcome);
                return Ok(());
            }
            let mode = output_mode(outcome.get("outputMode"));
            let content_type = output_content_type(outcome);
            let output = outcome.get("output").cloned().unwrap_or(Value::Null);
            let bytes = output_bytes(&output).map_err(spill_encode_response)?;
            if mode == WorkflowOutputMode::Inline && bytes.len() > WORKFLOW_INLINE_OUTPUT_CAP_BYTES {
                replace_with_step_output_limit_failure(outcome, WORKFLOW_INLINE_OUTPUT_CAP_BYTES);
                return Ok(());
            }
            if should_spill_output(mode, bytes.len()) {
                if bytes.len() as u64 > config.max_step_blob_bytes {
                    replace_with_step_output_limit_failure(outcome, config.max_step_blob_bytes as usize);
                    return Ok(());
                }
                let output_ref =
                    write_workflow_output_blob(config, app_id, &bytes, &content_type).await?;
                if let Some(obj) = outcome.as_object_mut() {
                    obj.remove("output");
                    obj.insert("outputRef".to_string(), output_ref);
                }
            }
            strip_output_mode_metadata(outcome);
            Ok(())
        }
        "RunCompleted" => {
            let output = outcome.get("output").cloned().unwrap_or(Value::Null);
            let bytes = output_bytes(&output).map_err(spill_encode_response)?;
            if bytes.len() > WORKFLOW_INLINE_OUTPUT_CAP_BYTES {
                if bytes.len() as u64 > config.max_step_blob_bytes {
                    replace_with_run_output_limit_failure(outcome, config.max_step_blob_bytes as usize);
                    return Ok(());
                }
                let output_ref =
                    write_workflow_output_blob(config, app_id, &bytes, "application/json").await?;
                if let Some(obj) = outcome.as_object_mut() {
                    obj.remove("output");
                    obj.insert("outputRef".to_string(), output_ref);
                }
            }
            strip_output_mode_metadata(outcome);
            Ok(())
        }
        _ => {
            strip_output_mode_metadata(outcome);
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowOutputMode {
    Auto,
    Inline,
    Blob,
    Stream,
}

fn output_mode(value: Option<&Value>) -> WorkflowOutputMode {
    match value.and_then(Value::as_str).unwrap_or("auto") {
        "inline" => WorkflowOutputMode::Inline,
        "blob" | "ref" => WorkflowOutputMode::Blob,
        "stream" => WorkflowOutputMode::Stream,
        _ => WorkflowOutputMode::Auto,
    }
}

fn output_content_type(outcome: &Value) -> String {
    outcome
        .get("outputContentType")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("application/json")
        .to_string()
}

fn should_spill_output(mode: WorkflowOutputMode, byte_len: usize) -> bool {
    matches!(mode, WorkflowOutputMode::Blob | WorkflowOutputMode::Stream)
        || byte_len > WORKFLOW_INLINE_OUTPUT_CAP_BYTES
}

fn output_bytes(output: &Value) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(output)
}

async fn write_workflow_output_blob(
    config: &WorkerConfig,
    app_id: &AppId,
    bytes: &[u8],
    content_type: &str,
) -> Result<Value, HttpResponse> {
    let hash = sha256_hex(bytes);
    config
        .workflow_blob_store
        .put_blob(&hash, bytes)
        .await
        .map_err(|e| {
            HttpResponse::ServiceUnavailable().json(&serde_json::json!({
                "error": format!("workflow output blob write failed: {e}")
            }))
        })?;
    cache::record_workflow_blob_write(app_id, bytes.len() as u64);
    Ok(serde_json::json!({
        "kind": "ref",
        "ref": format!("wfblob:sha256:{hash}"),
        "hash": hash,
        "size": bytes.len() as u64,
        "contentType": content_type,
    }))
}

fn strip_output_mode_metadata(outcome: &mut Value) {
    if let Some(obj) = outcome.as_object_mut() {
        obj.remove("outputMode");
        obj.remove("outputContentType");
    }
}

fn replace_with_step_output_limit_failure(outcome: &mut Value, limit: usize) {
    // maxStepBlobBytes is a hard platform cap (§3.5/§17): fail the run CLOSED with
    // LimitExceededError and journal NO step row. Emit a terminal RunFailed with
    // neither ordinal nor name — the §9 fold treats None+None as a terminal run
    // failure (Some(ordinal)+Some(name) would journal a retryable failed-step row,
    // leaving a blob-backed step behind for an aborted partial write).
    *outcome = serde_json::json!({
        "kind": "RunFailed",
        "error": workflow_output_limit_error(limit),
    });
}

fn replace_with_run_output_limit_failure(outcome: &mut Value, limit: usize) {
    *outcome = serde_json::json!({
        "kind": "RunFailed",
        "error": workflow_output_limit_error(limit),
    });
}

fn workflow_output_limit_error(limit: usize) -> Value {
    serde_json::json!({
        "type": "LimitExceededError",
        "message": format!("workflow output exceeds maxStepBlobBytes ({limit} bytes)"),
        "retryable": false,
    })
}

fn spill_encode_response(err: serde_json::Error) -> HttpResponse {
    HttpResponse::ServiceUnavailable().json(&serde_json::json!({
        "error": format!("encode workflow output before spill rewrite: {err}")
    }))
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
    app_id: AppId,
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
                let wall_delta = elapsed_us.saturating_sub(std::mem::replace(
                    &mut wall_recorded_us,
                    elapsed_us,
                ));
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
    app_id: &AppId,
) -> Result<(), String> {
    let app_version = crate::sync::fetch_app_version(&config.control_url, &config.service_auth, app_id).await?;

    let manifest = app_version
        .manifest
        .as_ref()
        .ok_or_else(|| format!("app {} has no manifest yet", app_id.as_str()))?;
    let bundle_hash = crate::sync::worker_entry_hash(manifest, app_id).ok_or_else(|| {
        format!(
            "app {} has no worker code (SSG-only or malformed manifest)",
            app_id.as_str()
        )
    })?;

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
    let env_json = crate::sync::fetch_app_env(&config.control_url, &config.service_auth, app_id)
        .await
        .map_err(|e| format!("env fetch failed: {e}"))?;

    // Now commit both atomically (env first so dispatchers always see
    // env present once runtime is present).
    if let Err(e) = crate::sync::put_env_from_json(envs, app_id.clone(), &env_json, app_version.env_version) {
        return Err(format!("env parse failed: {e}"));
    }
    let env_entry = crate::sync::get_env(envs, app_id)
        .ok_or_else(|| "env cache missing after env insert".to_string())?;
    // Resolve the bundled RuntimeSchemaDescriptor (if any) so the runtime
    // sources the schema from the generated descriptor. Absent descriptor means
    // schema-less app; expected-but-missing/corrupt descriptors are load errors.
    let descriptor_json = match crate::sync::runtime_descriptor_json(
        manifest,
        &config.blob_store,
        app_id,
    )
    .await
    {
        Ok(json) => json,
        Err(e) => {
            crate::sync::remove_env(envs, app_id);
            return Err(format!("descriptor load failed: {e}"));
        }
    };
    cache::load_app(
        app_id.clone(),
        &bytes,
        app_version.runtime.clone(),
        app_version.net_policy.clone(),
        app_version.deploy_hash.as_deref(),
        descriptor_json.as_deref(),
        manifest,
        &env_entry.snapshot,
    ).await
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
    cache::set_loaded_meta(app_id.clone(), cache::LoadedMeta {
        deploy_hash: app_version.deploy_hash.clone(),
        env_version: app_version.env_version,
        net_policy: app_version.net_policy,
    });
    tracing::info!(
        app_id = app_id.as_str(),
        blob_prefix = &bundle_hash[..bundle_hash.len().min(8)],
        "worker: on-demand loaded app"
    );
    Ok(())
}

async fn load_pinned_workflow_on_demand(
    config: &WorkerConfig,
    envs: &SharedEnvs,
    app_id: &AppId,
    deploy_hash: &str,
) -> Result<(), String> {
    let app_version = crate::sync::fetch_app_version(&config.control_url, &config.service_auth, app_id)
        .await
        .ok();
    let manifest_bytes = config
        .blob_store
        .get_manifest(app_id, deploy_hash)
        .await
        .map_err(|e| format!("manifest fetch failed: {e}"))?;
    let manifest: zeroship_bundle::Manifest = serde_json::from_slice(manifest_bytes.as_ref())
        .map_err(|e| format!("manifest parse failed: {e}"))?;
    if manifest.deploy_hash.as_deref() != Some(deploy_hash) {
        return Err(format!(
            "manifest deploy_hash mismatch: expected {deploy_hash}, got {:?}",
            manifest.deploy_hash
        ));
    }
    manifest
        .validate()
        .map_err(|e| format!("manifest validation failed: {e}"))?;

    let bundle_hash = crate::sync::worker_entry_hash(&manifest, app_id).ok_or_else(|| {
        format!(
            "app {} deploy {deploy_hash} has no worker code",
            app_id.as_str()
        )
    })?;
    let bytes = config
        .blob_store
        .get_blob(&bundle_hash)
        .await
        .map_err(|e| format!("blob fetch failed: {e}"))?;
    if bytes.is_empty() {
        return Err("empty bundle".into());
    }

    if crate::sync::get_env(envs, app_id).is_none()
        || app_version
            .as_ref()
            .is_some_and(|info| crate::sync::cached_env_version(envs, app_id) != Some(info.env_version))
    {
        let env_json = crate::sync::fetch_app_env(&config.control_url, &config.service_auth, app_id)
            .await
            .map_err(|e| format!("env fetch failed for pinned workflow load: {e}"))?;
        let env_version = app_version.as_ref().map_or(0, |info| info.env_version);
        crate::sync::put_env_from_json(envs, app_id.clone(), &env_json, env_version)
            .map_err(|e| format!("env parse failed for pinned workflow load: {e}"))?;
    }

    let env_entry = crate::sync::get_env(envs, app_id)
        .ok_or_else(|| "env unavailable for pinned workflow load".to_string())?;
    let descriptor_json = crate::sync::runtime_descriptor_json(&manifest, &config.blob_store, app_id)
        .await
        .map_err(|e| format!("descriptor load failed: {e}"))?;
    let runtime_limits = app_version
        .as_ref()
        .map_or_else(AppRuntimeLimits::default, |info| info.runtime.clone());
    let net_policy = app_version
        .as_ref()
        .map_or_else(AppNetPolicy::default, |info| info.net_policy.clone());

    cache::load_pinned_workflow_app(
        app_id.clone(),
        deploy_hash,
        &bytes,
        runtime_limits,
        net_policy,
        descriptor_json.as_deref(),
        &manifest,
        &env_entry.snapshot,
    ).await
    .map_err(|e| format!("failed to load pinned bundle: {e}"))?;

    tracing::info!(
        app_id = app_id.as_str(),
        deploy_hash = %deploy_hash,
        blob_prefix = &bundle_hash[..bundle_hash.len().min(8)],
        "worker: on-demand loaded pinned workflow app"
    );
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod workflow_tests;
