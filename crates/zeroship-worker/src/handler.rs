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
use zeroship_core::auth::derive_app_scoped_control_token;
use zeroship_core::service_identity::{endpoints, ServiceEndpoint};
use zeroship_core::service_peers::ServiceAuth;
use zeroship_core::dispatch_frame::decode_dispatch_frame;
use zeroship_core::types::{AppNetPolicy, AppRuntimeLimits};
use zeroship_bundle::sha256_hex;
use zeroship_plugin_workflow::advance::{
    collect_post_apply_registrations, worker_json_to_step_result, WorkflowAdvanceNackKind,
    WorkflowAdvanceResponse, WorkflowRunDispatchRequest,
};
use zeroship_plugin_workflow::apply;
use zeroship_plugin_workflow::claim::{
    claim_workflow_run, renew_workflow_claim, WorkflowClaimOutcome,
};
use zeroship_plugin_workflow::engine::{StepRequest, WorkflowEngineConfig};
use zeroship_plugin_workflow::errors::WorkflowError;
use zeroship_plugin_workflow::store::pg::PgStore;
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
static PROVISIONED_WORKFLOW_JOURNALS: OnceLock<Mutex<HashSet<Uuid>>> = OnceLock::new();

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
        // TRANSITIONAL, and the worker's ONLY conversion. Everything below this
        // line - the isolate cache, the env cache, the version feed, the meter,
        // the schema the app's tables live in - is keyed by the uuid the
        // control plane stores and serves, so the typed id is unwrapped here
        // rather than carried. Carrying it would silently re-key all of them to
        // a rendering no database row holds.
        Ok(id) => id.uuid(),
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
            let response =
                HttpResponse::NotFound().body(format!(r#"{{"error":"app {app_id} not loaded"}}"#));
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

fn workflow_runtime_envelope(
    config: &WorkerConfig,
    request: &StepRequest,
) -> serde_json::Value {
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
        "outputRead": {
            "controlUrl": &config.control_url,
            "token": derive_app_scoped_control_token(
                &config.control_key,
                &request.app_id.to_string(),
            ),
            "appId": request.app_id.to_string(),
        },
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
/// nowhere; only `tests/e2e_durable_workflows.sh` does.
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
    // typed id, and the uuid is what the journal, the claim and the pinned
    // isolate are keyed by.
    let app_id = match AppId::parse(path.as_str()) {
        Ok(id) => id.uuid(),
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
                "error": format!("app {app_id} deploy {} not loaded", claim.deploy_hash)
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

    let runtime_envelope = workflow_runtime_envelope(&config, &claim);
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
        claim.app_id,
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
            crate::logs::append(&logs, app_id, request_logs);
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
                    crate::logs::append(&logs, app_id, request_logs);
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
    app_id: Uuid,
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
                app_id,
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
        request.app_id,
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

    let store = PgStore::new(db_url.to_string(), request.app_id);
    let apply_config = workflow_apply_config_from_request(request);
    match apply::apply_step_result_on_store(&store, &apply_config, step_result).await {
        Ok(_applied) => match collect_post_apply_registrations(db_url, request.app_id, &request.run_id, true).await {
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

async fn ensure_workflow_journal_provisioned(db_url: &str, app_id: &Uuid) -> Result<(), String> {
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
        .insert(*app_id);
    Ok(())
}

async fn rewrite_workflow_output_blobs(
    config: &WorkerConfig,
    app_id: &Uuid,
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
    app_id: &Uuid,
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
    app_id: &Uuid,
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
    app_id: &Uuid,
) -> Result<(), String> {
    let app_version = crate::sync::fetch_app_version(&config.control_url, &config.service_auth, app_id).await?;

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
    let env_json = crate::sync::fetch_app_env(&config.control_url, &config.service_auth, app_id)
        .await
        .map_err(|e| format!("env fetch failed: {e}"))?;

    // Now commit both atomically (env first so dispatchers always see
    // env present once runtime is present).
    if let Err(e) = crate::sync::put_env_from_json(envs, *app_id, &env_json, app_version.env_version) {
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
        *app_id,
        &bytes,
        app_version.runtime.clone(),
        app_version.net_policy.clone(),
        app_version.deploy_hash.as_deref(),
        descriptor_json.as_deref(),
        manifest,
        &env_entry.snapshot,
    )
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
    cache::set_loaded_meta(*app_id, cache::LoadedMeta {
        deploy_hash: app_version.deploy_hash.clone(),
        env_version: app_version.env_version,
        net_policy: app_version.net_policy,
    });
    tracing::info!(
        app_id = %app_id,
        blob_prefix = &bundle_hash[..bundle_hash.len().min(8)],
        "worker: on-demand loaded app"
    );
    Ok(())
}

async fn load_pinned_workflow_on_demand(
    config: &WorkerConfig,
    envs: &SharedEnvs,
    app_id: &Uuid,
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

    let bundle_hash = crate::sync::worker_entry_hash(&manifest, app_id)
        .ok_or_else(|| format!("app {app_id} deploy {deploy_hash} has no worker code"))?;
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
        crate::sync::put_env_from_json(envs, *app_id, &env_json, env_version)
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
        *app_id,
        deploy_hash,
        &bytes,
        runtime_limits,
        net_policy,
        descriptor_json.as_deref(),
        &manifest,
        &env_entry.snapshot,
    )
    .map_err(|e| format!("failed to load pinned bundle: {e}"))?;

    tracing::info!(
        app_id = %app_id,
        deploy_hash = %deploy_hash,
        blob_prefix = &bundle_hash[..bundle_hash.len().min(8)],
        "worker: on-demand loaded pinned workflow app"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
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

    // `pub(super)` here and on `tmpdir` / `usage_value` below, so
    // `workflow_live_tests` at the foot of this file - a SIBLING module of this
    // one, not a child - can reach them. It is gated on a cargo feature and so
    // cannot live inside this module; those three are the only things it needs
    // from here, and duplicating them would be three more copies to keep in
    // step (one of them the V8 one-shot).
    pub(super) fn init_runtime() {
        V8_INIT.call_once(init_v8);
    }

    pub(super) fn tmpdir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "zs-worker-logs-{label}-{}",
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).expect("mkdir tmp");
        path
    }

    /// The path segment the worker's routes take: the app id in its printed,
    /// typed form.
    ///
    /// Tests hold the `Uuid` the control plane stores, and the worker's routes
    /// no longer read that spelling - the gateway renders the typed id and so
    /// must anything else addressing these endpoints. Spelling the uuid into
    /// the URL here would test a door the gateway never knocks on.
    pub(super) fn worker_app_path(app_id: &Uuid) -> String {
        zeroship_core::app_id::canonical_app_id_for(app_id)
            .as_str()
            .to_owned()
    }

    fn dispatch_frame(method: &str, url: &str, body: &[u8]) -> Vec<u8> {
        zeroship_core::dispatch_frame::encode_dispatch_frame(method, url, &[], body)
            .expect("dispatch frame")
    }

    pub(super) fn usage_value(
        events: &[zeroship_core::usage_event::UsageEvent],
        app_id: Uuid,
        meter: &str,
    ) -> Option<u64> {
        events
            .iter()
            .find(|event| event.subject.app == Some(app_id) && event.meter == meter)
            .map(|event| event.value)
    }

    struct MeteredDispatchResult {
        app_id: Uuid,
        status: StatusCode,
        body: Vec<u8>,
        events: Vec<zeroship_core::usage_event::UsageEvent>,
    }

    /// One worker identity for the whole test binary, plus the header the
    /// gateway presents to it.
    ///
    /// A process-wide `OnceLock` so every fixture below verifies under the SAME
    /// bundle that mints [`gateway_authorization`]. Two fixtures generating
    /// their own keys would each pass in isolation and fail the moment a test
    /// crossed between them, which is the shape hardest to read from a failing
    /// run.
    ///
    /// The TRANSPORT-ONLY profile, because that is what the worker runs on its
    /// dispatch hop: no `jti` claimed and no store consulted, so a single minted
    /// header is reusable across every request here. That reusability is not a
    /// test convenience - it is the property the tier buys, and a fixture that
    /// had to re-mint per request would be quietly measuring the wrong profile.
    /// Everything one worker identity carries for these tests.
    ///
    /// `gateway` and `impostor` are whole keyrings rather than bare headers
    /// because the identity envelope is signed per request: the forgery test
    /// needs a SECOND service that can produce a well-formed envelope the worker
    /// must refuse, and a fixture that only handed out strings could not express
    /// that at all.
    pub(crate) struct WorkerTestIdentity {
        pub(crate) service_auth: Arc<zeroship_core::service_peers::ServiceAuth>,
        pub(crate) gateway_header: String,
        pub(crate) control_header: String,
        /// The gateway's keyring - the ONLY one whose identity envelopes the
        /// worker's verifier is built to accept.
        pub(crate) gateway: zeroship_core::service_peers::ServiceKeyring,
        /// A second, differently-keyed service. Its assertions are not trusted
        /// and neither are its envelopes; it exists to be refused.
        pub(crate) impostor: zeroship_core::service_peers::ServiceKeyring,
    }

    fn worker_test_identity() -> &'static WorkerTestIdentity {
        use zeroship_core::service_assertion::{
            ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
        };
        use zeroship_core::service_peers::{
            service_issuer, ServiceAuth, ServiceKeyring, CONTROL_SERVICE_NAME,
            GATEWAY_SERVICE_NAME, WORKER_SERVICE_NAME,
        };
        use zeroship_core::user_envelope::UserEnvelopeVerifier;

        static IDENTITY: OnceLock<WorkerTestIdentity> = OnceLock::new();
        IDENTITY.get_or_init(|| {
            let worker_issuer = service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");
            let gateway_issuer = service_issuer(GATEWAY_SERVICE_NAME).expect("gateway issuer");
            let control_issuer = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
            let worker_key = ServiceSigningKey::generate();
            let gateway_key = ServiceSigningKey::generate();
            let control_key = ServiceSigningKey::generate();

            // BOTH peers, because the worker is a callee for two different
            // services on two different endpoints: the gateway dispatches, and
            // control reads app logs. Trusting one and testing the other is how
            // a fixture proves the wrong thing.
            // Two bundles carrying the same trust: a verifier consumes one.
            let mut trusted = ServiceTrustBundle::new();
            let mut held = ServiceTrustBundle::new();
            for bundle in [&mut trusted, &mut held] {
                bundle
                    .trust_signing_key(&gateway_issuer, gateway_key.key_id(), &gateway_key)
                    .expect("trust the gateway");
                bundle
                    .trust_signing_key(&control_issuer, control_key.key_id(), &control_key)
                    .expect("trust control");
            }

            // Built from `trusted` BEFORE the verifier consumes it, and for the
            // GATEWAY issuer alone. The worker's own key is in this fixture and
            // is deliberately not reachable through it: that is the asymmetry
            // `an_identity_envelope_signed_by_the_wrong_key_is_refused` rules on.
            let user_envelope = UserEnvelopeVerifier::for_issuer(&trusted, &gateway_issuer)
                .expect("the bundle publishes the gateway key");

            let keyring = ServiceKeyring::from_parts(worker_issuer, worker_key, held)
                .expect("worker keyring");
            let gateway = ServiceKeyring::from_parts(
                gateway_issuer.clone(),
                gateway_key,
                ServiceTrustBundle::new(),
            )
            .expect("gateway keyring");
            let control = ServiceKeyring::from_parts(
                control_issuer,
                control_key,
                ServiceTrustBundle::new(),
            )
            .expect("control keyring");
            // A fourth service, trusted by nobody. It signs under the GATEWAY's
            // issuer so a refusal cannot be attributed to a mismatched `iss`
            // string: the only thing wrong with its envelopes is the key.
            let impostor = ServiceKeyring::from_parts(
                gateway_issuer.clone(),
                ServiceSigningKey::generate(),
                ServiceTrustBundle::new(),
            )
            .expect("impostor keyring");
            let worker_audience = service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");
            let gateway_header = format!(
                "Bearer {}",
                gateway.mint_for(&worker_audience).expect("mint for the worker")
            );
            let control_header = format!(
                "Bearer {}",
                control.mint_for(&worker_audience).expect("mint for the worker")
            );
            WorkerTestIdentity {
                service_auth: Arc::new(
                    ServiceAuth::new(keyring, Arc::new(TransportAssertionVerifier::new(trusted)))
                        .verifying_user_envelopes(user_envelope),
                ),
                gateway_header,
                control_header,
                gateway,
                impostor,
            }
        })
    }

    /// The gateway's credential for the worker, for tests that drive dispatch.
    pub(crate) fn gateway_authorization() -> &'static str {
        &worker_test_identity().gateway_header
    }

    /// Control's credential for the worker. The log read is granted to
    /// `svc/control` and NOT to `svc/gateway`, so the two headers are not
    /// interchangeable - which is the separation a shared bearer could not
    /// express and this fixture would hide if it minted only one.
    fn control_authorization() -> &'static str {
        &worker_test_identity().control_header
    }

    pub(crate) fn test_service_auth() -> Arc<zeroship_core::service_peers::ServiceAuth> {
        Arc::clone(&worker_test_identity().service_auth)
    }

    /// A worker config pointed at a dead control plane, so any on-demand load
    /// attempt fails rather than reaching the network.
    fn test_worker_config(blob_root: &std::path::Path) -> Arc<crate::WorkerConfig> {
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(LocalDiskBlobStore::new(blob_root.to_path_buf()).expect("blob store"));
        let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
            zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.to_path_buf())
                .expect("workflow blob store"),
        );
        Arc::new(crate::WorkerConfig {
            service_auth: test_service_auth(),
            control_url: "http://127.0.0.1:1".to_string(),
            control_key: String::new(),
            db_url: None,
            kv_url: None,
            storage_backend: None,
            max_isolates: 10,
            max_pinned_isolates_per_app: 4,
            poll_interval_secs: 60,
            shutdown_timeout_secs: 0,
            blob_store,
            workflow_blob_store,
            max_step_blob_bytes: 64 * 1024 * 1024,
            workflow_advance_unsigned: false,
        })
    }

    /// Drive `dispatch` with a raw payload against an app that was never
    /// loaded, so the request is rejected before any isolate work happens.
    /// These are the pre-dispatch reject arms; the app id is well-formed, so
    /// the rejected request is still attributable to an app.
    fn run_pre_dispatch_reject(payload: Vec<u8>) -> Option<MeteredDispatchResult> {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        Some(runtime.block_on(async {
            let app_id = Uuid::new_v4();
            let meter = Arc::new(zeroship_metering::Meter::new());
            crate::cache::init_cache(
                10,
                4,
                crate::cache::KernelConfig {
                    control_url: "http://127.0.0.1:1".to_string(),
                    control_key: String::new(),
                    db_service: None,
                    kv_url: None,
                    storage_backend: None,
                    meter: meter.clone(),
                },
            );

            let envs: SharedEnvs = Arc::new(RwLock::new(HashMap::new()));
            let logs = crate::logs::new_store();
            let blob_root = tmpdir("pre-dispatch-reject-meter");
            let config = test_worker_config(&blob_root);

            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .configure(configure),
            )
            .await;

            let req = test::TestRequest::post()
                .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(payload)
                .to_request();
            let resp = test::call_service(&app, req).await;
            let status = resp.status();
            let body = test::read_body(resp).await.to_vec();
            let events = meter.drain();

            let _ = std::fs::remove_dir_all(blob_root);
            MeteredDispatchResult { app_id, status, body, events }
        }))
    }

    /// Drive `dispatch` with a LITERAL path segment rather than a well-formed
    /// id, and report the status alongside how far the bad-app-id counter
    /// moved. The counter is the half that says WHICH refusal happened: a 400
    /// is also what a malformed frame answers, and that arm increments a
    /// different counter.
    fn dispatch_path_segment(segment: &str) -> (StatusCode, u64) {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        runtime.block_on(async {
            let meter = Arc::new(zeroship_metering::Meter::new());
            crate::cache::init_cache(
                10,
                4,
                crate::cache::KernelConfig {
                    control_url: "http://127.0.0.1:1".to_string(),
                    control_key: String::new(),
                    db_service: None,
                    kv_url: None,
                    storage_backend: None,
                    meter,
                },
            );

            let envs: SharedEnvs = Arc::new(RwLock::new(HashMap::new()));
            let logs = crate::logs::new_store();
            let blob_root = tmpdir("dispatch-path-segment");
            let config = test_worker_config(&blob_root);

            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .configure(configure),
            )
            .await;

            let before = metrics::DISPATCH_REJECTED_BAD_APP_ID
                .load(std::sync::atomic::Ordering::Relaxed);
            let req = test::TestRequest::post()
                .uri(&format!("/dispatch/{segment}"))
                .header("authorization", gateway_authorization())
                .set_payload(dispatch_frame("GET", "http://app.test/", b""))
                .to_request();
            let resp = test::call_service(&app, req).await;
            let status = resp.status();
            let after = metrics::DISPATCH_REJECTED_BAD_APP_ID
                .load(std::sync::atomic::Ordering::Relaxed);

            let _ = std::fs::remove_dir_all(blob_root);
            (status, after - before)
        })
    }

    /// The dispatch path segment carries an APP ID, and a uuid rendering is not
    /// one.
    ///
    /// RED BEFORE THIS SLICE, and it failed on both halves: `path.parse::<Uuid>()`
    /// accepted the hyphenated form, so the request walked on to the env lookup
    /// and answered 503 while the counter never moved.
    ///
    /// The gateway is the only producer of this path, and it now renders
    /// [`zeroship_core::app_id::AppId::as_str`]. A hyphenated uuid arriving
    /// here is therefore a producer/consumer disagreement about the RENDERING
    /// of the identity, which is the failure this whole migration exists to
    /// make loud: `Uuid::parse_str` and `AppId::parse` accept disjoint strings,
    /// so the day the two sides disagree the request is refused at the door
    /// rather than dispatched to a tenant nobody meant.
    #[test]
    fn a_hyphenated_uuid_is_refused_on_the_dispatch_path() {
        let (status, rejected) = dispatch_path_segment(&Uuid::new_v4().to_string());
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a uuid rendering is not an app id and must not reach dispatch",
        );
        assert!(
            rejected >= 1,
            "the refusal must be attributed to the bad app id, not to the frame",
        );
    }

    /// The control for the arm above, differing in ONE variable: the rendering.
    ///
    /// Same uuid, same frame, same wiring - only the printed form changes. It
    /// must NOT be refused as a bad app id, or the arm above would pass just as
    /// well against a door that is shut to everyone.
    #[test]
    fn the_canonical_rendering_of_the_same_id_passes_the_door() {
        let raw = Uuid::new_v4();
        let canonical = zeroship_core::app_id::canonical_app_id_for(&raw);
        let (status, rejected) = dispatch_path_segment(canonical.as_str());
        assert_eq!(
            rejected, 0,
            "the canonical rendering must not be refused as a bad app id",
        );
        assert_ne!(
            status,
            StatusCode::BAD_REQUEST,
            "the canonical rendering must get past the app-id check; it fails \
             later, on the env lookup, because this app was never loaded",
        );
    }

    /// A body over the cap is rejected by US, with our status and our shape.
    ///
    /// The size check in `dispatch` was unreachable in production for as long
    /// as the route carried no `PayloadConfig`: ntex applied its own 256 KiB
    /// default and answered a bare 400 before the handler ran. So this test is
    /// only meaningful because it builds the app through `configure`, the same
    /// registration the server uses. Wiring the route by hand here would put
    /// the default back and quietly assert the opposite of production.
    ///
    /// 413 rather than 400 is the whole point: it proves the frame reached the
    /// handler and was refused on the DECODED body length.
    #[test]
    fn oversized_request_body_is_rejected_by_the_handler_not_the_extractor() {
        let body = vec![b'x'; MAX_DISPATCH_BODY_BYTES + 1];
        let frame = dispatch_frame("POST", "http://app.test/", &body);
        let Some(result) = run_pre_dispatch_reject(frame) else {
            return;
        };
        assert_eq!(
            result.status,
            ntex::http::StatusCode::PAYLOAD_TOO_LARGE,
            "a body one byte over the cap must reach the handler and get our \
             413, not the extractor's 400; body was {}",
            String::from_utf8_lossy(&result.body),
        );
    }

    /// The counterpart: a body at the cap is admitted past the size check.
    ///
    /// Without this the test above would still pass if the cap were zero, or if
    /// the extractor limit were set below the body cap - both of which reject
    /// everything. This pins that the accepted side of the boundary is real,
    /// and with it that the frame allowance genuinely exceeds the body cap by
    /// the envelope overhead.
    #[test]
    fn body_at_the_cap_passes_the_size_check() {
        let body = vec![b'x'; MAX_DISPATCH_BODY_BYTES];
        let frame = dispatch_frame("POST", "http://app.test/", &body);
        let Some(result) = run_pre_dispatch_reject(frame) else {
            return;
        };
        assert_ne!(
            result.status,
            ntex::http::StatusCode::PAYLOAD_TOO_LARGE,
            "a body exactly at the cap must not be refused for size",
        );
        assert_ne!(
            result.status,
            ntex::http::StatusCode::BAD_REQUEST,
            "a body exactly at the cap must not be refused by the extractor; \
             the frame allowance has to exceed the body cap by the envelope",
        );
    }

    fn run_metered_dispatch(
        source: &[u8],
        limits: AppRuntimeLimits,
        request_body: &[u8],
        insert_env: bool,
    ) -> Option<MeteredDispatchResult> {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        Some(runtime.block_on(async {
            init_runtime();

            let app_id = Uuid::new_v4();
            let meter = Arc::new(zeroship_metering::Meter::new());
            crate::cache::init_cache(
                10,
                4,
                crate::cache::KernelConfig {
                    control_url: "http://127.0.0.1:1".to_string(),
                    control_key: String::new(),
                    db_service: None,
                    kv_url: None,
                    storage_backend: None,
                    meter: meter.clone(),
                },
            );
            crate::cache::load_app(
                app_id,
                source,
                limits,
                zeroship_core::types::AppNetPolicy::default(),
                None,
                None,
                &zeroship_bundle::Manifest::default(),
                &EnvSnapshot::empty(),
            )
            .expect("app loads");

            let envs: SharedEnvs = Arc::new(RwLock::new(HashMap::new()));
            if insert_env {
                crate::sync::put_env_from_json(
                    &envs,
                    app_id,
                    r#"{"vars":{},"secrets":{},"expose":[]}"#,
                    0,
                )
                .expect("insert env");
            }
            let logs = crate::logs::new_store();
            let blob_root = tmpdir("generated-error-meter");
            let config = test_worker_config(&blob_root);

            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .configure(configure),
            )
            .await;

            let req = test::TestRequest::post()
                .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(dispatch_frame(
                    "POST",
                    "http://example.test/generated-error",
                    request_body,
                ))
                .to_request();
            let resp = test::call_service(&app, req).await;
            let status = resp.status();
            let body = test::read_body(resp).await.to_vec();
            let events = meter.drain();

            let _ = std::fs::remove_dir_all(blob_root);
            MeteredDispatchResult { app_id, status, body, events }
        }))
    }

    fn assert_generated_error_metering(result: MeteredDispatchResult, status: StatusCode) {
        assert_eq!(result.status, status);
        assert!(!result.body.is_empty(), "generated error body must be non-empty");
        assert_eq!(
            usage_value(&result.events, result.app_id, "requests"),
            Some(1),
            "generated error must count exactly one request"
        );
        assert_eq!(
            usage_value(&result.events, result.app_id, "egress_bytes"),
            Some(result.body.len() as u64),
            "generated error egress must equal the response body length"
        );
    }

    #[test]
    fn dispatch_meters_unsupported_upgrade_error_body() {
        let source = br#"
            export default {
              fetch() {
                const pair = new WebSocketPair();
                const [client, server] = Object.values(pair);
                server.accept();
                return new Response(null, { status: 101, webSocket: client });
              }
            };
        "#;
        let Some(result) =
            run_metered_dispatch(source, AppRuntimeLimits::default(), b"sync-upgrade", true)
        else {
            return;
        };

        assert_generated_error_metering(result, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn dispatch_meters_settled_unsupported_upgrade_error_body() {
        let source = br#"
            export default {
              async fetch() {
                await new Promise(resolve => setTimeout(resolve, 0));
                const pair = new WebSocketPair();
                const [client, server] = Object.values(pair);
                server.accept();
                return new Response(null, { status: 101, webSocket: client });
              }
            };
        "#;
        let Some(result) =
            run_metered_dispatch(source, AppRuntimeLimits::default(), b"settled-upgrade", true)
        else {
            return;
        };

        assert_generated_error_metering(result, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn dispatch_meters_pending_dispatch_error_body() {
        let source = br#"
            export default {
              async fetch() {
                await new Promise(resolve => setTimeout(resolve, 0));
                return null;
              }
            };
        "#;
        let Some(result) =
            run_metered_dispatch(source, AppRuntimeLimits::default(), b"pending-error", true)
        else {
            return;
        };

        assert_generated_error_metering(result, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn dispatch_meters_timeout_error_body() {
        let source = br#"
            export default {
              fetch() {
                return new Promise(() => {});
              }
            };
        "#;
        let limits = AppRuntimeLimits {
            wall_timeout_ms: Some(10),
            ..AppRuntimeLimits::default()
        };
        let Some(result) = run_metered_dispatch(source, limits, b"timeout", true) else {
            return;
        };

        assert_generated_error_metering(result, StatusCode::GATEWAY_TIMEOUT);
    }

    /// CPU burned AFTER the first await must land on the `cpu_us` meter.
    ///
    /// The handler returns a pending promise straight away, so the
    /// synchronous isolate entry this dispatch path times around
    /// `call_fetch_handler_with_user` sees almost nothing. All the work
    /// happens in the timer continuation, which V8 runs on the runtime's
    /// pump — and pump CPU used to reach no meter at all, so an app that does
    /// its work in promise chains, `setInterval` callbacks or stream pushes
    /// was billed as if it were idle. `RuntimeInner::bill_pump_cpu` now emits
    /// each pump V8 window to the app's `cpu_us` meter, which is keyed by app
    /// — the granularity billing consumes — even though the work is not
    /// attributable to any one request.
    ///
    /// `setTimeout(fn, 0)` specifically lands in `RuntimeState::ready_timers`
    /// and fires inline in the pump's PHASE 1 drain, a window the per-app CPU
    /// budget never saw either. Keeping the delay at 0 here is therefore
    /// deliberate: it exercises the arm that had no accounting whatsoever.
    ///
    /// The burn is wall-clock driven inside JS but it is a spin loop, so the
    /// thread CPU clock the pump samples tracks it. The assertion floor is
    /// well under the burn to absorb scheduling noise while staying far
    /// above the few milliseconds of module init that the synchronous entry
    /// legitimately contributes.
    #[test]
    fn dispatch_meters_cpu_burned_on_the_pump_after_an_await() {
        let source = br#"
            export default {
              async fetch() {
                await new Promise(resolve => setTimeout(resolve, 0));
                const deadline = Date.now() + 300;
                while (Date.now() < deadline) {}
                return new Response("burned");
              }
            };
        "#;
        let Some(result) =
            run_metered_dispatch(source, AppRuntimeLimits::default(), b"pump-cpu", true)
        else {
            return;
        };

        assert_eq!(
            result.status,
            StatusCode::OK,
            "the burn handler must complete normally; body was {}",
            String::from_utf8_lossy(&result.body),
        );
        assert_eq!(
            usage_value(&result.events, result.app_id, "requests"),
            Some(1),
            "exactly one request",
        );

        let cpu_us = usage_value(&result.events, result.app_id, "cpu_us").unwrap_or(0);
        assert!(
            cpu_us >= 200_000,
            "a 300 ms spin loop that runs on the pump must be metered as \
             cpu_us; got {cpu_us} us, which means the pump's V8 window burned \
             the app's CPU without billing it",
        );
    }

    /// A `setTimeout(fn, 0)` callback that re-arms ITSELF must not wedge the pump.
    ///
    /// The zero-delay queue (`RuntimeState::ready_timers`) is drained by the
    /// pump's PHASE 1. That drain used to run until the queue was empty, and a
    /// callback that re-arms itself pushes onto the very queue the drain pops
    /// from — so `spin(){ burn(); setTimeout(spin, 0) }` never let the drain
    /// return. Every mechanism that would have noticed sits after it:
    /// `bill_pump_cpu` on the next line, `record_pump_cpu` further down the same
    /// iteration. The chain therefore pinned a core while being billed nothing
    /// and policed by nothing, and — the part this test pins — the pump never
    /// got back to its event loop, so unrelated I/O on the same isolate stalled
    /// forever behind it.
    ///
    /// The handler below encodes exactly that. The response is gated on a REAL
    /// (>= 1 ms) timer, which is the half that makes the failure observable: a
    /// delay of >= 1 ms lands in `spawned_timers` -> `AsyncWork::pending_timers`,
    /// and the ONLY place those are polled is the pump's event select, after
    /// the drain returns. With an unbounded drain the response is unreachable.
    ///
    /// WHAT IS ASSERTED, and why not termination. The 80%-of-wall budget in
    /// `record_pump_cpu` is fed only from the pump's PHASE 2 window, so a chain
    /// that lives entirely in PHASE 1 is outside its input by construction —
    /// asserting on termination would be asserting on a mechanism this fix
    /// deliberately does not rewire. Billing is the mechanism that IS supposed
    /// to see this window, so the assertions are (a) the dispatch completes at
    /// all, which is the liveness bug itself, and (b) the CPU the chain burned
    /// reaches `cpu_us`.
    ///
    /// The JS chain is UNBOUNDED on purpose — a chain with a stop condition
    /// gets billed correctly even today, just late, because the drain does
    /// eventually return. So the wall-time bound has to come from the harness
    /// rather than from the app: the dispatch runs on its own thread with its
    /// own compio runtime (the isolate cache is thread-local), and the test
    /// waits on a channel with a timeout. A wedged pump is then a failed
    /// assertion with a diagnosis, not a hung suite. On that failure path the
    /// worker thread is left spinning and deliberately not joined — joining it
    /// is precisely the hang being avoided; it dies with the process.
    #[test]
    fn self_rescheduling_zero_delay_timer_does_not_wedge_the_pump() {
        const WATCHDOG: Duration = Duration::from_secs(30);

        let source = br#"
            export default {
              async fetch() {
                // Re-arms itself every hop, so `ready_timers` is never empty.
                function spin() {
                  const until = Date.now() + 4;
                  while (Date.now() < until) {}
                  setTimeout(spin, 0);
                }
                setTimeout(spin, 0);
                // A real timer: reachable only via the pump's event select,
                // i.e. only if the zero-delay drain hands control back.
                await new Promise(resolve => setTimeout(resolve, 5));
                return new Response("pump-alive");
              }
            };
        "#;

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_metered_dispatch(
                source,
                AppRuntimeLimits::default(),
                b"pump-spin",
                true,
            ));
        });

        let Ok(dispatched) = rx.recv_timeout(WATCHDOG) else {
            panic!(
                "the pump never came back: a self-rescheduling setTimeout(fn, 0) \
                 chain held the PHASE 1 drain for {WATCHDOG:?}, so the handler's \
                 5 ms timer was never polled and the request could not complete. \
                 The zero-delay drain must be bounded per pass.",
            );
        };
        let Some(result) = dispatched else {
            return;
        };

        assert_eq!(
            result.status,
            StatusCode::OK,
            "the handler must complete normally once the pump yields between \
             passes; body was {}",
            String::from_utf8_lossy(&result.body),
        );
        assert_eq!(result.body, b"pump-alive");

        let cpu_us = usage_value(&result.events, result.app_id, "cpu_us").unwrap_or(0);
        assert!(
            cpu_us >= 25_000,
            "the CPU a self-rescheduling zero-delay chain burns on the pump must \
             reach cpu_us; got {cpu_us} us. Zero here means the drain ran the \
             app's JS and returned past `bill_pump_cpu` without it, which is the \
             under-billing half of the same bug",
        );
    }

    #[test]
    fn pre_dispatch_bad_envelope_reject_records_platform_counters() {
        // The frame does not decode, so the inner body length is unknowable and
        // ingress is the raw bytes the worker actually received off the wire.
        let payload = b"not-a-dispatch-frame".to_vec();
        let Some(result) = run_pre_dispatch_reject(payload.clone()) else {
            return;
        };

        assert_eq!(result.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            usage_value(&result.events, result.app_id, "requests"),
            Some(1),
            "a rejected envelope is still one request the platform served"
        );
        assert_eq!(
            usage_value(&result.events, result.app_id, "ingress_bytes"),
            Some(payload.len() as u64),
            "ingress must be the raw frame bytes received, since the frame did not decode"
        );
        assert_eq!(
            usage_value(&result.events, result.app_id, "egress_bytes"),
            Some(result.body.len() as u64),
            "egress must be the error body actually sent"
        );
    }

    #[test]
    fn pre_dispatch_on_demand_load_failure_records_platform_counters() {
        // The control plane is unreachable, so on-demand load fails and the
        // request is refused before any isolate exists.
        let request_body = b"load-failure-request-body";
        let payload = dispatch_frame("POST", "http://example.test/load-fail", request_body);
        let Some(result) = run_pre_dispatch_reject(payload) else {
            return;
        };

        assert_eq!(result.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            usage_value(&result.events, result.app_id, "requests"),
            Some(1),
            "a failed on-demand load is still one request the platform handled"
        );
        assert_eq!(
            usage_value(&result.events, result.app_id, "ingress_bytes"),
            Some(request_body.len() as u64),
            "ingress must be the request body the worker received"
        );
    }

    #[test]
    fn dispatch_cached_runtime_missing_env_records_all_five_platform_counters() {
        let source = br#"
            export default {
              fetch() {
                return new Response("unreachable");
              }
            };
        "#;
        let request_body = b"missing-env-request-body";
        let Some(result) = run_metered_dispatch(
            source,
            AppRuntimeLimits::default(),
            request_body,
            false,
        ) else {
            return;
        };

        assert_eq!(result.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!result.body.is_empty(), "503 body must be non-empty");
        assert_eq!(
            usage_value(&result.events, result.app_id, "requests"),
            Some(1),
            "missing env must count exactly one request"
        );
        assert_eq!(
            usage_value(&result.events, result.app_id, "ingress_bytes"),
            Some(request_body.len() as u64),
            "missing env ingress must equal the request body length"
        );
        assert!(
            usage_value(&result.events, result.app_id, "wall_us").unwrap_or(0) > 0,
            "missing env wall time must be recorded"
        );
        assert_eq!(
            usage_value(&result.events, result.app_id, "cpu_us").unwrap_or(0),
            0,
            "missing env does not enter V8 and must record zero V8 CPU time"
        );
        assert_eq!(
            usage_value(&result.events, result.app_id, "egress_bytes"),
            Some(result.body.len() as u64),
            "missing env egress must equal the 503 body length"
        );
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
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

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
                4,
                crate::cache::KernelConfig {
                    control_url: "http://127.0.0.1:1".to_string(),
                    control_key: String::new(),
                    db_service: None,
                    kv_url: None,
                    storage_backend: None,
                    meter: std::sync::Arc::new(zeroship_metering::Meter::new()),
                },
            );
            crate::cache::load_app(
                app_id,
                source,
                AppRuntimeLimits::default(),
                zeroship_core::types::AppNetPolicy::default(),
                None,
                None,
                &zeroship_bundle::Manifest::default(),
                &EnvSnapshot::empty(),
            )
            .expect("app loads");

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
            let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
                zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
                    .expect("workflow blob store"),
            );
            let config = Arc::new(crate::WorkerConfig {
                service_auth: test_service_auth(),
                control_url: "http://127.0.0.1:1".to_string(),
                control_key: String::new(),
                db_url: None,
                kv_url: None,
                storage_backend: None,
                max_isolates: 10,
                max_pinned_isolates_per_app: 4,
                poll_interval_secs: 60,
                shutdown_timeout_secs: 0,
                blob_store,
                workflow_blob_store,
                max_step_blob_bytes: 64 * 1024 * 1024,
                workflow_advance_unsigned: false,
            });

            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .configure(configure)
                    .service(
                        web::resource("/logs/{app_id}")
                            .route(web::get().to(crate::logs::get_logs)),
                    ),
            )
            .await;

            let req = test::TestRequest::post()
                .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(dispatch_frame(
                    "GET",
                    "http://example.test/from-worker-test",
                    b"",
                ))
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            assert_eq!(&body[..], b"ok");

            let req = test::TestRequest::get()
                .uri(&format!("/logs/{}", worker_app_path(&app_id)))
                .header("authorization", control_authorization())
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            let lines: Vec<String> = serde_json::from_slice(&body).expect("logs json");
            assert_eq!(lines, vec!["b2-real-log /from-worker-test"]);

            let _ = std::fs::remove_dir_all(blob_root);
        });
    }

    // ── The worker must enforce the DECLARED auth policy ──────────────────
    //
    // The two tests below are a matched pair - same fixture, same app, same
    // dispatch, ONE variable changed: the level the deployed manifest
    // declares for the route being called. The `anonymous` arm must be served, the
    // `user` arm must be refused. Neither is meaningful without the other:
    // an arm that only asserted the refusal would also pass on a worker that
    // 401s everything.
    //
    // WHY THEY EXIST. `RequiredPrincipal` occurred ZERO times in this crate and
    // zero times in `zeroship-runtime`; the gateway was the only tier that read
    // the declared policy. The worker's own dispatch path handled identity
    // exactly once - `verified_user_json` above, which HMAC-verifies a
    // `ZeroShip-User` header when one is present and returns `Ok(None)` when
    // it is not. `None` was then passed on and the handler ran. So a request
    // that arrived with no identity was served on a route declared `user`
    // unless the creator's own JS remembered to call `env.auth.requireUser()`
    // - a creator-called, optional gate. A creator who forgot had no fence
    // at all.
    //
    // `router/dispatch.rs` states the assumption in prose: "Resource-tree
    // `user` routes are already short-circuited by `resolve_auth` upstream,
    // so they never reach the worker." That is a statement about the gateway's
    // current behaviour, not an invariant the worker enforces, and it is the
    // thing these tests refuse to accept on trust.
    //
    // WHAT WAS MISSING, EXACTLY. Not the policy - the manifest already reached
    // this crate. `zeroship_core::types` carries `manifest: Option<Manifest>`
    // in the version feed and `sync.rs` reads it on every reload
    // (`worker_entry_hash`, `runtime_descriptor_json`). What was missing was
    // the hand-off: `cache::load_app` took limits, a net policy, a deploy hash,
    // a runtime descriptor and an env snapshot, and NOT the resource policy -
    // so by the time `dispatch` ran there was nothing to consult. It now takes
    // the manifest, compiles it once per load, and `crate::policy::enforce`
    // reads it per dispatch.
    //
    // NOTE ON THE FIXTURE MANIFEST. It is a real `zeroship_bundle::Manifest`
    // parsed from the wire JSON, not a hand-built struct, so a change to the
    // resource wire shape breaks these tests rather than passing them.

    /// The declaration under test: `/api/private` requires a user,
    /// `/api/public` does not. Parsed from the wire form the build emits.
    fn manifest_declaring_a_user_route() -> zeroship_bundle::Manifest {
        let manifest: zeroship_bundle::Manifest = serde_json::from_value(serde_json::json!({
            "version": 1,
            "resources": {
                "/api/private": { "auth": "user" },
                "/api/public": { "auth": "anonymous", "publicly_accessible": true },
            }
        }))
        .expect("fixture manifest parses as the real wire shape");

        // Guard the fixture: if this ever stops saying `user`, the tests
        // below would be ruling on a route nobody declared.
        assert_eq!(
            manifest
                .resources
                .get("/api/private")
                .and_then(|entry| entry.auth),
            Some(zeroship_bundle::RequiredPrincipal::User),
            "fixture must declare /api/private as a user route"
        );
        assert_eq!(
            manifest
                .resources
                .get("/api/public")
                .and_then(|entry| entry.auth),
            Some(zeroship_bundle::RequiredPrincipal::Anonymous),
            "fixture must declare /api/public as an anonymous route"
        );
        manifest
    }

    /// App code that records that it ran, so a test can tell "refused before
    /// creator code" apart from "creator code ran and happened to 401".
    /// It calls NOTHING from `env.auth` - that is the point: enforcement must
    /// not depend on the creator remembering.
    const AUTH_OBLIVIOUS_APP: &[u8] = br#"
        export default {
          fetch(req) {
            console.log("creator-code-ran", new URL(req.url).pathname);
            return new Response("served");
          }
        }
    "#;

    /// Boot a worker with `AUTH_OBLIVIOUS_APP` loaded and dispatch `path`
    /// with NO `ZeroShip-User` header at all. Returns the status the worker
    /// answered and the console lines the app produced.
    async fn dispatch_unauthenticated(
        manifest: &zeroship_bundle::Manifest,
        path: &str,
    ) -> (StatusCode, Vec<String>) {
        init_runtime();

        let app_id = Uuid::new_v4();
        crate::cache::init_cache(
            10,
            4,
            crate::cache::KernelConfig {
                control_url: "http://127.0.0.1:1".to_string(),
                control_key: String::new(),
                db_service: None,
                kv_url: None,
                storage_backend: None,
                meter: std::sync::Arc::new(zeroship_metering::Meter::new()),
            },
        );
        // The declared policy travels the last hop with the bundle it belongs
        // to, so what the fixture declared above is what `dispatch` rules on.
        crate::cache::load_app(
            app_id,
            AUTH_OBLIVIOUS_APP,
            AppRuntimeLimits::default(),
            zeroship_core::types::AppNetPolicy::default(),
            None,
            None,
            manifest,
            &EnvSnapshot::empty(),
        )
        .expect("app loads");

        let envs: SharedEnvs = Arc::new(RwLock::new(HashMap::new()));
        crate::sync::put_env_from_json(&envs, app_id, r#"{"vars":{},"secrets":{},"expose":[]}"#, 0)
            .expect("insert env");
        let logs = crate::logs::new_store();
        let blob_root = tmpdir("declared-auth-policy");
        let config = test_worker_config(&blob_root);

        let app = test::init_service(
            web::App::new()
                .state(config)
                .state(envs)
                .state(logs.clone())
                .configure(configure),
        )
        .await;

        // No `ZeroShip-User`, no `Authorization`, no cookie. This is exactly
        // what a caller with direct network access to the worker sends, and
        // what the gateway forwards on a public route.
        let req = test::TestRequest::post()
            .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
            .set_payload(dispatch_frame(
                "GET",
                &format!("http://app.test{path}"),
                b"",
            ))
            .to_request();
        let resp = test::call_service(&app, req).await;
        let status = resp.status();
        let _ = test::read_body(resp).await;
        let lines = crate::logs::get(&logs, &app_id);

        let _ = std::fs::remove_dir_all(blob_root);
        (status, lines)
    }

    /// THE DEFECT. A route the deploy declares `auth: "user"`, called with no
    /// identity at all, must be refused by the worker in Rust before the
    /// creator's handler is entered.
    #[test]
    fn dispatch_refuses_an_unauthenticated_request_to_a_user_route() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        runtime.block_on(async {
            let manifest = manifest_declaring_a_user_route();
            let (status, lines) = dispatch_unauthenticated(&manifest, "/api/private").await;

            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "a `user` route reached with no identity must be refused by the \
                 worker; got {status} instead, which means the request was served"
            );
            assert!(
                lines.is_empty(),
                "the refusal must happen BEFORE creator code runs, so the app \
                 must have logged nothing; got {lines:?}"
            );
        });
    }

    /// The dispatch hop refuses a caller that has not proved WHICH SERVICE it
    /// is.
    ///
    /// The predecessor accepted every caller whenever the shared secret was
    /// empty, so this arm could not be written at all: the fixtures all ran
    /// with `worker_key: String::new()`, which was the disabled state. The
    /// three headers below are the shapes an attacker with network reach can
    /// produce without the gateway's private key.
    #[test]
    fn dispatch_refuses_a_caller_with_no_service_credential() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let dir = tmpdir("dispatch-no-credential");
            let config = test_worker_config(&dir);
            let app_id = Uuid::new_v4();
            let app = test::init_service(
                web::App::new()
                    .state(config.clone())
                    .state(crate::sync::SharedEnvs::default())
                    .state(crate::logs::new_store())
                    .configure(configure),
            )
            .await;

            for header in [None, Some("Bearer "), Some("Bearer not-an-assertion")] {
                let mut req = test::TestRequest::post()
                    .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                    .set_payload(dispatch_frame("GET", "http://example.test/", b""));
                if let Some(value) = header {
                    req = req.header("authorization", value);
                }
                let resp = test::call_service(&app, req.to_request()).await;
                assert_eq!(
                    resp.status(),
                    StatusCode::UNAUTHORIZED,
                    "header {header:?} must not reach the runtime"
                );
            }

            // THE CONTROL, one variable apart: with the gateway's assertion the
            // request gets past the guard and fails on the missing app instead,
            // so the refusals above are the credential check rather than the
            // route being unreachable in this fixture.
            let resp = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                    .header("authorization", gateway_authorization())
                    .set_payload(dispatch_frame("GET", "http://example.test/", b""))
                    .to_request(),
            )
            .await;
            assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
        });
    }

    /// An unconfigured worker refuses dispatch instead of admitting everyone.
    ///
    /// The exact inversion of the deleted `if worker_key.is_empty() { return
    /// None }`, asserted directly because it is the property a later "make it
    /// work locally" edit is most likely to undo.
    #[test]
    fn an_unconfigured_worker_refuses_dispatch() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let dir = tmpdir("dispatch-unconfigured");
            let mut config = test_worker_config(&dir);
            Arc::get_mut(&mut config).expect("sole owner").service_auth =
                Arc::new(zeroship_core::service_peers::ServiceAuth::unconfigured());
            let app_id = Uuid::new_v4();
            let app = test::init_service(
                web::App::new()
                    .state(config.clone())
                    .state(crate::sync::SharedEnvs::default())
                    .state(crate::logs::new_store())
                    .configure(configure),
            )
            .await;

            // Even the gateway's real assertion is refused: with no peer bundle
            // there is nothing to verify it against, and the answer is no.
            let resp = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                    .header("authorization", gateway_authorization())
                    .set_payload(dispatch_frame("GET", "http://example.test/", b""))
                    .to_request(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        });
    }

    /// THE RED TEST OF THE ASYMMETRIC ENVELOPE: an identity envelope signed
    /// with the WRONG key is refused, and the request never reaches the runtime.
    ///
    /// # Its existence is the proof the keys actually split
    ///
    /// This test COULD NOT BE WRITTEN before this change, and not because nobody
    /// tried. The envelope was an HMAC keyed by `worker_key` - the same secret
    /// the worker used to verify it - so the worker could produce any envelope
    /// it could check. "Signed with the wrong key" had no referent: there was
    /// one key, and anything that failed to verify was a corrupted header rather
    /// than a forgery by a distinct party. What made the arm expressible is that
    /// signing and verifying are now different capabilities held by different
    /// processes.
    ///
    /// The impostor signs under the GATEWAY's own issuer, so the refusal cannot
    /// be a mismatched `iss` string. It is the signature, and nothing else.
    #[test]
    fn an_identity_envelope_signed_by_the_wrong_key_is_refused() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let dir = tmpdir("dispatch-forged-identity");
            let config = test_worker_config(&dir);
            let app_id = Uuid::new_v4();
            let app = test::init_service(
                web::App::new()
                    .state(config.clone())
                    .state(crate::sync::SharedEnvs::default())
                    .state(crate::logs::new_store())
                    .configure(configure),
            )
            .await;

            const USER: &[u8] = br#"{"id":"pws_forged","email":"a@b.test","name":"A","avatar":null,"email_verified":true,"scopes":[]}"#;
            let request_id = Uuid::new_v4();
            let identity = worker_test_identity();
            let forged = identity
                .impostor
                .user_envelope_signer()
                .sign(USER, request_id);

            let resp = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                    .header("authorization", gateway_authorization())
                    .header("x-request-id", request_id.to_string().as_str())
                    .header("zeroship-user", forged.as_str())
                    .set_payload(dispatch_frame("GET", "http://example.test/", b""))
                    .to_request(),
            )
            .await;
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "an identity envelope the gateway did not sign must be refused, \
                 even when the TRANSPORT credential is the gateway's genuine one"
            );

            // THE SAME FORGERY, RELABELLED with the gateway's real `kid`.
            //
            // Without this arm the one above is bound by KEY RESOLUTION rather
            // than by the signature: an unknown `kid` resolves to no key and is
            // refused before any verification runs, so neutralising the
            // signature check leaves it green. Measured, not assumed - deleting
            // `verify_strict` from `UserEnvelopeVerifier` kept the arm above
            // passing and this one is what goes red.
            let mut parts: Vec<&str> = forged.split('.').collect();
            parts[3] = identity.gateway.user_envelope_signer().key_id();
            let relabelled = parts.join(".");
            let resp = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                    .header("authorization", gateway_authorization())
                    .header("x-request-id", request_id.to_string().as_str())
                    .header("zeroship-user", relabelled.as_str())
                    .set_payload(dispatch_frame("GET", "http://example.test/", b""))
                    .to_request(),
            )
            .await;
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "naming a trusted kid must not admit an envelope signed by another key"
            );

            // THE CONTROL, one variable apart: the SAME payload, the SAME
            // request id, the SAME transport credential - signed by the gateway.
            // It gets past the identity check and fails on the missing app
            // instead, so the refusals above are the credential and not the
            // shape of the envelope or the reachability of the route.
            let genuine = identity.gateway.user_envelope_signer().sign(USER, request_id);
            assert_ne!(genuine, forged, "the two signers must differ");
            let resp = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                    .header("authorization", gateway_authorization())
                    .header("x-request-id", request_id.to_string().as_str())
                    .header("zeroship-user", genuine.as_str())
                    .set_payload(dispatch_frame("GET", "http://example.test/", b""))
                    .to_request(),
            )
            .await;
            assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
        });
    }

    /// The worker cannot mint an identity it would accept.
    ///
    /// The corollary of the arm above, asserted against the WORKER's own service
    /// key rather than a third party's - because that is the key the worker
    /// actually holds, and the one a compromise of the worker process hands an
    /// attacker. Under the shared secret this was the whole defect; here the
    /// worker's key is published under a different issuer, so the verifier
    /// resolves no key for it.
    #[test]
    fn the_worker_own_service_key_cannot_sign_an_identity_it_accepts() {
        let identity = worker_test_identity();
        const USER: &[u8] = br#"{"id":"pws_self","email":"a@b.test","name":"A","avatar":null,"email_verified":true,"scopes":[]}"#;
        let request_id = Uuid::new_v4();
        let verifier = identity
            .service_auth
            .user_envelope_verifier()
            .expect("the test worker verifies envelopes");

        let self_signed = identity
            .service_auth
            .user_envelope_signer()
            .expect("the worker holds its own key")
            .sign(USER, request_id);
        assert_eq!(
            verifier.verify_for_request(&self_signed, request_id),
            None,
            "the worker must not be able to mint an identity it would accept"
        );

        // The one-variable partner: the gateway's signature over the same bytes
        // for the same request IS accepted.
        assert!(verifier
            .verify_for_request(
                &identity.gateway.user_envelope_signer().sign(USER, request_id),
                request_id
            )
            .is_some());
    }

    /// THE CONTROL. Same app, same missing identity, one variable changed:
    /// the route is declared `anonymous`. It must still be served, or the arm
    /// above would pass on a worker that simply refuses everything.
    #[test]
    fn dispatch_still_serves_an_unauthenticated_request_to_an_anonymous_route() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        runtime.block_on(async {
            let manifest = manifest_declaring_a_user_route();
            let (status, lines) = dispatch_unauthenticated(&manifest, "/api/public").await;

            assert_eq!(
                status,
                StatusCode::OK,
                "an `anonymous` route must stay reachable without identity"
            );
            assert_eq!(
                lines,
                vec!["creator-code-ran /api/public".to_string()],
                "the app's handler must have run on the anonymous route"
            );
        });
    }

    #[test]
    fn dispatch_preserves_non_utf8_request_body_bytes() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        runtime.block_on(async {
            init_runtime();

            let app_id = Uuid::new_v4();
            let source = br#"
                export default {
                  async fetch(req) {
                    const bytes = Array.from(new Uint8Array(await req.arrayBuffer()));
                    return Response.json({ bytes });
                  }
                }
            "#;
            crate::cache::init_cache(
                10,
                4,
                crate::cache::KernelConfig {
                    control_url: "http://127.0.0.1:1".to_string(),
                    control_key: String::new(),
                    db_service: None,
                    kv_url: None,
                    storage_backend: None,
                    meter: std::sync::Arc::new(zeroship_metering::Meter::new()),
                },
            );
            crate::cache::load_app(
                app_id,
                source,
                AppRuntimeLimits::default(),
                zeroship_core::types::AppNetPolicy::default(),
                None,
                None,
                &zeroship_bundle::Manifest::default(),
                &EnvSnapshot::empty(),
            )
            .expect("app loads");

            let envs: SharedEnvs = Arc::new(RwLock::new(HashMap::new()));
            crate::sync::put_env_from_json(
                &envs,
                app_id,
                r#"{"vars":{},"secrets":{},"expose":[]}"#,
                0,
            )
            .expect("insert env");
            let logs = crate::logs::new_store();
            let blob_root = tmpdir("blob-binary-body");
            let blob_store: Arc<dyn BlobStore> =
                Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
            let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
                zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
                    .expect("workflow blob store"),
            );
            let config = Arc::new(crate::WorkerConfig {
                service_auth: test_service_auth(),
                control_url: "http://127.0.0.1:1".to_string(),
                control_key: String::new(),
                db_url: None,
                kv_url: None,
                storage_backend: None,
                max_isolates: 10,
                max_pinned_isolates_per_app: 4,
                poll_interval_secs: 60,
                shutdown_timeout_secs: 0,
                blob_store,
                workflow_blob_store,
                max_step_blob_bytes: 64 * 1024 * 1024,
                workflow_advance_unsigned: false,
            });

            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .configure(configure),
            )
            .await;

            let raw_body = [0xff, 0x00, 0xfe, 0x80];
            let req = test::TestRequest::post()
                .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(dispatch_frame(
                    "POST",
                    "http://example.test/binary-body",
                    &raw_body,
                ))
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            let v: serde_json::Value =
                serde_json::from_slice(&body).expect("handler returned JSON");
            assert_eq!(
                v["bytes"],
                serde_json::json!([255, 0, 254, 128]),
                "request body must be byte-exact, not UTF-8-lossy"
            );

            let _ = std::fs::remove_dir_all(blob_root);
        });
    }

    #[test]
    fn dispatch_preserves_non_utf8_response_body_bytes() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        runtime.block_on(async {
            init_runtime();

            let app_id = Uuid::new_v4();
            let source = br#"
                export default {
                  fetch() {
                    return new Response(new Uint8Array([0xFF, 0x00, 0xFE, 0x80]));
                  }
                }
            "#;
            crate::cache::init_cache(
                10,
                4,
                crate::cache::KernelConfig {
                    control_url: "http://127.0.0.1:1".to_string(),
                    control_key: String::new(),
                    db_service: None,
                    kv_url: None,
                    storage_backend: None,
                    meter: std::sync::Arc::new(zeroship_metering::Meter::new()),
                },
            );
            crate::cache::load_app(
                app_id,
                source,
                AppRuntimeLimits::default(),
                zeroship_core::types::AppNetPolicy::default(),
                None,
                None,
                &zeroship_bundle::Manifest::default(),
                &EnvSnapshot::empty(),
            )
            .expect("app loads");

            let envs: SharedEnvs = Arc::new(RwLock::new(HashMap::new()));
            crate::sync::put_env_from_json(
                &envs,
                app_id,
                r#"{"vars":{},"secrets":{},"expose":[]}"#,
                0,
            )
            .expect("insert env");
            let logs = crate::logs::new_store();
            let blob_root = tmpdir("blob-binary-response");
            let blob_store: Arc<dyn BlobStore> =
                Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
            let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
                zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
                    .expect("workflow blob store"),
            );
            let config = Arc::new(crate::WorkerConfig {
                service_auth: test_service_auth(),
                control_url: "http://127.0.0.1:1".to_string(),
                control_key: String::new(),
                db_url: None,
                kv_url: None,
                storage_backend: None,
                max_isolates: 10,
                max_pinned_isolates_per_app: 4,
                poll_interval_secs: 60,
                shutdown_timeout_secs: 0,
                blob_store,
                workflow_blob_store,
                max_step_blob_bytes: 64 * 1024 * 1024,
                workflow_advance_unsigned: false,
            });

            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .configure(configure),
            )
            .await;

            let req = test::TestRequest::post()
                .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(dispatch_frame(
                    "GET",
                    "http://example.test/binary-response",
                    b"",
                ))
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            assert_eq!(
                &body[..],
                &[0xff, 0x00, 0xfe, 0x80],
                "response body must be byte-exact, not UTF-8-lossy"
            );

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
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

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
                4,
                crate::cache::KernelConfig {
                    control_url: "http://127.0.0.1:1".to_string(),
                    control_key: String::new(),
                    db_service: None,
                    kv_url: None,
                    storage_backend: None,
                    meter: meter.clone(),
                },
            );
            crate::cache::load_app(
                app_id,
                source,
                AppRuntimeLimits::default(),
                zeroship_core::types::AppNetPolicy::default(),
                None,
                None,
                &zeroship_bundle::Manifest::default(),
                &EnvSnapshot::empty(),
            )
            .expect("app loads");

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
            let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
                zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
                    .expect("workflow blob store"),
            );
            let config = Arc::new(crate::WorkerConfig {
                service_auth: test_service_auth(),
                control_url: "http://127.0.0.1:1".to_string(),
                control_key: String::new(),
                db_url: None,
                kv_url: None,
                storage_backend: None,
                max_isolates: 10,
                max_pinned_isolates_per_app: 4,
                poll_interval_secs: 60,
                shutdown_timeout_secs: 0,
                blob_store,
                workflow_blob_store,
                max_step_blob_bytes: 64 * 1024 * 1024,
                workflow_advance_unsigned: false,
            });

            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .configure(configure),
            )
            .await;

            // A non-empty request body → ingress_bytes must equal its length.
            let req_body = "the-end-user-request-body-payload";
            let url = "http://example.test/counters-probe";
            let req = test::TestRequest::post()
                .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(dispatch_frame("POST", url, req_body.as_bytes()))
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            let resp_body_len = body.len() as u64;
            assert!(resp_body_len > 0, "handler returned a non-empty body");

            // Drain the SAME meter the handler fed — the faithful assertion.
            let events = meter.drain();

            assert_eq!(
                usage_value(&events, app_id, "requests"),
                Some(1),
                "requests counter unchanged"
            );
            assert_eq!(
                usage_value(&events, app_id, "ingress_bytes"),
                Some(req_body.len() as u64),
                "ingress_bytes must equal the request body length"
            );
            assert_eq!(
                usage_value(&events, app_id, "egress_bytes"),
                Some(resp_body_len),
                "egress_bytes must equal the response body length"
            );
            assert!(
                usage_value(&events, app_id, "wall_us").unwrap_or(0) > 0,
                "wall_us must be a positive elapsed-time measurement"
            );
            assert!(
                usage_value(&events, app_id, "cpu_us").unwrap_or(0) > 0,
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
    /// Service dependency: a single-node Redis, resolved from the test overlay
    /// that `tests/provision_test_backends.sh` writes, or from `REDIS_TEST_URL`
    /// overriding it. REQUIRED, not optional. Storage + db + auth need no
    /// external service.
    ///
    /// This used to skip when `REDIS_TEST_URL` was unset unless
    /// `KV_REQUIRE_REDIS=1` turned the skip into a failure, "in CI". Nothing in
    /// this repository ever set `KV_REQUIRE_REDIS` - not a workflow, not a
    /// script - so the panic was unreachable and the skip was the only
    /// behaviour, matching `crates/zeroship-plugin-kv/tests/redis_backend.rs`, which had
    /// the same dead flag and the same untrue comment. Both are resolved the
    /// way `ZEROSHIP_REQUIRE_LIVE_BACKENDS` was: the flag is deleted and Redis
    /// is simply required, because the provisioner now supplies it.
    #[test]
    fn dispatch_resolves_full_kernel_kv_storage_db_auth() {
        let kv_url = zeroship_core::config::test_kv_url();

        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

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
                4,
                crate::cache::KernelConfig {
                    control_url: "http://127.0.0.1:1".to_string(),
                    control_key: String::new(),
                    // Dummy DSN: the service validates and stores the URL and
                    // the backend connects lazily, so `env.db` is installed
                    // without a live Postgres.
                    db_service: Some(crate::cache::test_db_service(
                        "postgres://localhost/zs_phase2_unused",
                        "handler-test-worker",
                    )),
                    kv_url: Some(kv_url),
                    storage_backend: Some(StorageBackendConfig::Local(storage_root.clone())),
                    meter: std::sync::Arc::new(zeroship_metering::Meter::new()),
                },
            );
            crate::cache::load_app(
                app_id,
                source,
                AppRuntimeLimits::default(),
                zeroship_core::types::AppNetPolicy::default(),
                None,
                None,
                &zeroship_bundle::Manifest::default(),
                &EnvSnapshot::empty(),
            )
            .expect("app loads");

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
            let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
                zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
                    .expect("workflow blob store"),
            );
            let config = Arc::new(crate::WorkerConfig {
                service_auth: test_service_auth(),
                control_url: "http://127.0.0.1:1".to_string(),
                control_key: String::new(),
                db_url: Some("postgres://localhost/zs_phase2_unused".to_string()),
                kv_url: None,
                storage_backend: None,
                max_isolates: 10,
                max_pinned_isolates_per_app: 4,
                poll_interval_secs: 60,
                shutdown_timeout_secs: 0,
                blob_store,
                workflow_blob_store,
                max_step_blob_bytes: 64 * 1024 * 1024,
                workflow_advance_unsigned: false,
            });

            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .configure(configure),
            )
            .await;

            let req = test::TestRequest::post()
                .uri(&format!("/dispatch/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(dispatch_frame(
                    "GET",
                    "http://example.test/kernel-probe",
                    b"",
                ))
                .to_request();
            let resp = test::call_service(&app, req).await;
            // Read the body BEFORE asserting the status. A bare status assertion
            // here reports "expected 200, got 500" and discards the one thing that
            // says which namespace failed and why.
            let status = resp.status();
            let body = test::read_body(resp).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "full-kernel dispatch must succeed; a missing env.* namespace 500s. body: {}",
                String::from_utf8_lossy(&body)
            );
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
            4,
            crate::cache::KernelConfig {
                control_url: "http://127.0.0.1:1".to_string(),
                control_key: String::new(),
                db_service: None,
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
        let rt = compio::runtime::Runtime::new().expect("compio runtime");
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
            let events = meter.drain();
            assert_eq!(
                usage_value(&events, app_id, "egress_bytes"),
                Some(pushed),
                "the mid-stream byte-threshold flush records the streamed bytes \
                 before finalize"
            );
            // `unwrap_or(0) >= 0` on a `u64` is always true regardless of whether the
            // metric was ever recorded (clippy::absurd_extreme_comparisons), which
            // silently defeated the "is recorded" claim below. Assert presence instead.
            assert!(
                usage_value(&events, app_id, "stream_wall_us").is_some(),
                "stream_wall_us is recorded as a custom metric on the incremental flush"
            );
            // The drain task NEVER counts `requests` (a stream is one request,
            // counted by record_stream_unary — not exercised here).
            assert_eq!(
                usage_value(&events, app_id, "requests"),
                None,
                "stream_response must not touch requests"
            );

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

            // `drain()` reset after the first read, so this second drain holds
            // only the post-first-drain deltas (the second batch + any final
            // wall delta).
            let events2 = meter.drain();
            assert_eq!(
                usage_value(&events2, app_id, "egress_bytes"),
                Some(more_len),
                "the final delta records the remaining streamed bytes"
            );
            assert_eq!(
                usage_value(&events2, app_id, "requests"),
                None,
                "still no requests from the drain"
            );
        });
    }

    /// `requests` is counted EXACTLY ONCE for a stream (a stream is one
    /// request), by `record_stream_unary` at stream start — never by the
    /// per-delta drain. This pins that the unary recorder bumps `requests`
    /// once and the drain bumps it zero times, so N incremental deltas can
    /// never inflate the request count.
    #[test]
    fn streaming_counts_request_exactly_once() {
        let rt = compio::runtime::Runtime::new().expect("compio runtime");
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

            let events = meter.drain();
            assert_eq!(
                usage_value(&events, app_id, "requests"),
                Some(1),
                "a stream counts exactly one request despite many incremental deltas"
            );
            assert_eq!(
                usage_value(&events, app_id, "cpu_us"),
                Some(123),
                "unary cpu_us recorded once"
            );
            assert_eq!(
                usage_value(&events, app_id, "ingress_bytes"),
                Some(456),
                "unary ingress_bytes recorded once"
            );
            assert!(
                usage_value(&events, app_id, "egress_bytes").unwrap_or(0) > 0,
                "incremental egress accrued across deltas"
            );
            assert!(
                usage_value(&events, app_id, "stream_wall_us").unwrap_or(0) > 0
                    || usage_value(&events, app_id, "egress_bytes").unwrap_or(0) > 0,
                "stream_wall_us accrues over the stream lifetime"
            );
        });
    }

    // -----------------------------------------------------------------------
    // In-flight dispatch must pin its isolate against LRU eviction
    // -----------------------------------------------------------------------

    /// A dispatch that is still awaiting user code makes its isolate
    /// un-evictable, and a competing load that would have evicted it is
    /// refused instead.
    ///
    /// The bug this pins: the isolate cache is thread-local and this executor
    /// is single-threaded, so every await inside `dispatch` hands the thread to
    /// whatever else is queued. If that other work loads a different app while
    /// the cache is at `max_size`, `evict_lru` picks the least-recently-used
    /// entry - which can be the isolate the parked dispatch is still driving.
    /// Eviction closes that isolate's native sockets and fires all of its
    /// in-flight `AbortController`s, so the running request is destroyed
    /// underneath itself.
    ///
    /// `evict_lru` has always filtered on `Runtime::is_isolate_leased()`, but
    /// nothing on the dispatch path ever took a lease - the counter was
    /// incremented only by a cache unit test - so the filter was a permanent
    /// no-op and this test is RED without the `lease_isolate()` binding in
    /// `dispatch`. Deleting that one line makes the first assertion below fail
    /// with "dispatch never took an isolate lease".
    ///
    /// It has to run through the real HTTP surface (`configure` + the ntex test
    /// service) for the same reason `evict_lru`'s own unit test proves nothing:
    /// what is under test is whether the DISPATCH PATH takes the lease, not
    /// whether `lease_isolate` increments a counter.
    ///
    /// `leased == true` is itself the in-flight proof: the guard is an RAII
    /// local in `dispatch`, so it exists only between "isolate resolved" and
    /// "response returned". Observing it set means the request had genuinely
    /// not finished when the competing load ran.
    #[test]
    fn in_flight_dispatch_pins_its_isolate_against_eviction() {
        let rt = compio::runtime::Runtime::new().expect("compio runtime");

        rt.block_on(async {
            init_runtime();

            let app_a = Uuid::new_v4();
            let app_b = Uuid::new_v4();

            // max_size = 1: loading app B MUST try to evict app A.
            crate::cache::init_cache(
                1,
                4,
                crate::cache::KernelConfig {
                    control_url: "http://127.0.0.1:1".to_string(),
                    control_key: String::new(),
                    db_service: None,
                    kv_url: None,
                    storage_backend: None,
                    meter: Arc::new(zeroship_metering::Meter::new()),
                },
            );

            // App A parks on a timer, so its dispatch is demonstrably still in
            // flight (awaiting the pending-promise channel) while we try to
            // load app B on the same thread.
            let slow = br#"
                export default {
                  async fetch() {
                    await new Promise((r) => setTimeout(r, 300));
                    return new Response("a-finished");
                  }
                }
            "#;
            let fast = br#"
                export default { fetch() { return new Response("b"); } }
            "#;

            crate::cache::load_app(
                app_a,
                slow,
                AppRuntimeLimits::default(),
                zeroship_core::types::AppNetPolicy::default(),
                None,
                None,
                &zeroship_bundle::Manifest::default(),
                &EnvSnapshot::empty(),
            )
            .expect("app A loads");

            let envs: SharedEnvs = Arc::new(RwLock::new(HashMap::new()));
            crate::sync::put_env_from_json(
                &envs,
                app_a,
                r#"{"vars":{},"secrets":{},"expose":[]}"#,
                0,
            )
            .expect("insert env A");
            let logs = crate::logs::new_store();
            let blob_root = tmpdir("in-flight-lease");
            let config = test_worker_config(&blob_root);

            let service = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .configure(configure),
            )
            .await;

            // Drive A's dispatch concurrently with this task.
            let dispatch = compio::runtime::spawn(async move {
                let req = test::TestRequest::post()
                    .uri(&format!("/dispatch/{}", worker_app_path(&app_a)))
                .header("authorization", gateway_authorization())
                    .set_payload(dispatch_frame("GET", "http://app-a.test/", b""))
                    .to_request();
                let resp = test::call_service(&service, req).await;
                let status = resp.status();
                let body = test::read_body(resp).await.to_vec();
                (status, body)
            });

            // Yield until A's dispatch has entered the handler and parked on
            // the timer. Bounded: if it never reports leased we fail below
            // rather than spin forever.
            let mut leased = false;
            for _ in 0..2000 {
                if crate::cache::get_runtime(&app_a)
                    .is_some_and(|r| r.is_isolate_leased())
                {
                    leased = true;
                    break;
                }
                let _ = compio::runtime::spawn(async {}).await;
            }

            assert!(
                leased,
                "dispatch never took an isolate lease: an in-flight request \
                 leaves its isolate a legal LRU eviction victim, and eviction \
                 closes its sockets and fires its AbortControllers mid-request"
            );

            // The competing load. With A leased this must be REFUSED, not
            // served by destroying A.
            let load_b = crate::cache::load_app(
                app_b,
                fast,
                AppRuntimeLimits::default(),
                zeroship_core::types::AppNetPolicy::default(),
                None,
                None,
                &zeroship_bundle::Manifest::default(),
                &EnvSnapshot::empty(),
            );

            let err = load_b.expect_err(
                "loading a second app into a full cache whose only isolate is \
                 running a request must be deferred, not satisfied by evicting \
                 the running isolate",
            );
            assert!(
                err.contains("leased"),
                "the deferral must be the documented 'every isolate is leased' \
                 refusal, got: {err}"
            );

            // A is still cached - it was not evicted - and still running.
            assert!(
                crate::cache::get_runtime(&app_a).is_some(),
                "the in-flight app must still be cached after the refused load"
            );

            // And it completes normally, its sockets and abort signals intact.
            let (status, body) = dispatch.await.expect("dispatch task did not panic");
            assert_eq!(
                status,
                ntex::http::StatusCode::OK,
                "in-flight dispatch must finish normally: {}",
                String::from_utf8_lossy(&body),
            );
            assert_eq!(
                String::from_utf8_lossy(&body),
                "a-finished",
                "the parked promise resolved and its response survived"
            );

            // Lease released on return: the isolate is evictable again, so the
            // pin cannot leak and permanently wedge the cache.
            assert!(
                crate::cache::get_runtime(&app_a)
                    .is_some_and(|r| !r.is_isolate_leased()),
                "the RAII lease must be released when the dispatch returns"
            );

            let _ = std::fs::remove_dir_all(blob_root);
        });
    }
}

// ---------------------------------------------------------------------------
// The live-PostgreSQL half of this module's tests.
//
// WHY THESE ARE NOT IN `mod tests` ABOVE. Every test here drives
// `workflow_advance_unsigned` end to end, and that path claims a run with
//
//     JOIN zeroship.apps  JOIN zeroship.plans  JOIN zeroship.app_deploys
//
// (crates/zeroship-plugin-workflow/src/claim.rs:89-91). Those are PLATFORM
// tables, created by the committed migration corpus in `db/migrations-ts/` -
// not by any fixture. A database that is merely REACHABLE is not enough, and
// the difference is invisible in the failure: pointed at the shared, unmigrated
// `zeroship` database these seven died on
//
//     relation "zeroship.plans" does not exist
//
// which cargo prints as seven ordinary FAILED lines. That is a VOID RUN dressed
// as a regression - `zeroship-control` hit the identical shape on 2026-08-20
// (see the `live-db-tests` block in crates/zeroship-control/Cargo.toml) and the
// answer there is the answer here.
//
// WHAT RUNS THEM. `tests/run_worker_suite.sh`, which creates and migrates a
// database named after this tree's migration set and then invokes
//
//     cargo test -p zeroship-worker --features live-db-tests
//
// It also counts how many of these actually reported `ok`, so gating them out
// of the default build cannot silently become gating them out of everything.
// CI runs that script as the `worker-live-gate` job.
//
// WHAT A DEFAULT `cargo test -p zeroship-worker` THEREFORE MEANS. It rules on
// the worker's request path, config, cache and boot posture and says NOTHING
// about workflow advance. That is the honest reading, and it is why the
// suite script exists rather than a tolerated red.
// ---------------------------------------------------------------------------
#[cfg(all(test, feature = "live-db-tests"))]
mod workflow_live_tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    use compio_postgres::NoTls;
    use ntex::http::StatusCode;
    use ntex::web::{self, test};
    use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
    use zeroship_migrate_server::provisioning::provision_workflow_journal_schema;
    use zeroship_plugin_workflow::store::pg::{PgStore, WorkflowTables};

    use super::tests::{init_runtime, tmpdir, usage_value, worker_app_path};
    use super::*;

    const WORKFLOW_TEST_PLAN: &str = "pln_worker_workflow_test";

    fn append_tar_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder
            .append_data(&mut header, path, std::io::Cursor::new(bytes))
            .expect("append tar file");
    }

    fn workflow_source(mark: &str) -> Vec<u8> {
        r#"
const MARK = "__MARK__";

export class Checkout {
  async run(trigger, step) {
    const first = await step.run("first", () => {
      globalThis.__bodyRuns = (globalThis.__bodyRuns ?? 0) + 1;
      return { mark: MARK, step: "first", bodyRuns: globalThis.__bodyRuns, input: trigger.input };
    });
    const second = await step.run("second", () => {
      globalThis.__bodyRuns = (globalThis.__bodyRuns ?? 0) + 1;
      return { mark: MARK, step: "second", bodyRuns: globalThis.__bodyRuns, first };
    });
    await step.sleep("nap", "PT1S");
    return { mark: MARK, second };
  }
}

export class ConcurrentWorkflow {
  async run(trigger, step) {
    const values = await Promise.all([
      step.run("a", () => ({ mark: MARK, step: "a", input: trigger.input })),
      step.run("b", () => ({ mark: MARK, step: "b", input: trigger.input })),
      step.run("c", () => ({ mark: MARK, step: "c", input: trigger.input })),
    ]);
    const final = await step.run("final", () => ({ mark: MARK, values }));
    return { values, final };
  }
}

export default { workflows: { Checkout, ConcurrentWorkflow } };
"#
        .replace("__MARK__", mark)
        .into_bytes()
    }

    fn workflow_zship(source: &[u8]) -> Vec<u8> {
        let source_hash = zeroship_bundle::sha256_hex(source);
        let manifest = serde_json::json!({
            "version": 1,
            "worker": {
                "entry": "index.js",
                "modules": { "index.js": source_hash },
            },
            "resources": {},
            "assets": {},
            "runtime_assets": {},
            "asset_version": 0,
            "sourcemaps": {},
            "metadata": { "built_at": "2026-07-06T00:00:00Z" },
        });
        let manifest_bytes = serde_json::to_vec(&manifest).expect("manifest json");
        let mut builder = tar::Builder::new(Vec::new());
        append_tar_file(&mut builder, "manifest.json", &manifest_bytes);
        append_tar_file(&mut builder, &format!("blobs/{source_hash}"), source);
        let tar_bytes = builder.into_inner().expect("tar bytes");
        zstd::stream::encode_all(std::io::Cursor::new(tar_bytes), 0).expect("zstd encode")
    }

    async fn deploy_workflow_fixture(
        blob_store: &Arc<dyn BlobStore>,
        app_id: &Uuid,
        mark: &str,
    ) -> String {
        let source = workflow_source(mark);
        let zship = workflow_zship(&source);
        zeroship_bundle::ingest(blob_store, app_id, &zship)
            .await
            .expect("workflow zship ingest")
            .deploy_hash
    }

    fn workflow_request(app_id: &Uuid) -> serde_json::Value {
        workflow_request_for_run(app_id, "run_test")
    }

    fn workflow_request_for_run(app_id: &Uuid, run_id: &str) -> serde_json::Value {
        serde_json::json!({
            "runId": run_id,
            "appId": app_id,
        })
    }

    // Postgres is not optional for this workspace's tests (see
    // crates/zeroship-test-support/src/lib.rs); the workflow-apply tests below cannot
    // do without it, so this panics with the provisioning command rather than
    // let them report a pass for a check they never ran.
    fn workflow_test_db_url() -> String {
        zeroship_core::config::test_database_url()
    }

    // Generic test-state setup (`workflow_test_state[_with_meter]`) is shared
    // by tests that do not touch the database at all, so it keeps tolerating
    // an absent overlay rather than forcing Postgres on every caller.
    fn workflow_test_db_url_opt() -> Option<String> {
        zeroship_core::config::test_database_url_opt()
    }

    // (app_id, blob_store, envs, logs, config, blob_root)
    type WorkflowTestState = (
        Uuid,
        Arc<dyn BlobStore>,
        SharedEnvs,
        crate::logs::SharedLogs,
        Arc<crate::WorkerConfig>,
        PathBuf,
    );

    fn workflow_test_state(max_pinned_isolates_per_app: usize) -> WorkflowTestState {
        let (app_id, blob_store, envs, logs, config, _meter, blob_root) =
            workflow_test_state_with_meter(max_pinned_isolates_per_app);
        (app_id, blob_store, envs, logs, config, blob_root)
    }

    // (app_id, blob_store, envs, logs, config, meter, blob_root)
    type WorkflowTestStateWithMeter = (
        Uuid,
        Arc<dyn BlobStore>,
        SharedEnvs,
        crate::logs::SharedLogs,
        Arc<crate::WorkerConfig>,
        Arc<zeroship_metering::Meter>,
        PathBuf,
    );

    fn workflow_test_state_with_meter(max_pinned_isolates_per_app: usize) -> WorkflowTestStateWithMeter {
        init_runtime();
        let app_id = Uuid::new_v4();
        let meter = Arc::new(zeroship_metering::Meter::new());
        let db_url = workflow_test_db_url_opt();
        crate::cache::init_cache(
            10,
            max_pinned_isolates_per_app,
            crate::cache::KernelConfig {
                control_url: "http://127.0.0.1:1".to_string(),
                control_key: String::new(),
                db_service: db_url
                    .as_deref()
                    .map(|url| crate::cache::test_db_service(url, "handler-test-worker")),
                kv_url: None,
                storage_backend: None,
                meter: meter.clone(),
            },
        );
        let envs: SharedEnvs = Arc::new(RwLock::new(HashMap::new()));
        crate::sync::put_env_from_json(
            &envs,
            app_id,
            r#"{"vars":{},"secrets":{},"expose":[]}"#,
            0,
        )
        .expect("insert workflow env");
        let logs = crate::logs::new_store();
        let blob_root = tmpdir("workflow-blob");
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
        let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
            zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
                .expect("workflow blob store"),
        );
        let config = Arc::new(crate::WorkerConfig {
            service_auth: test_service_auth(),
            control_url: "http://127.0.0.1:1".to_string(),
            control_key: String::new(),
            db_url,
            kv_url: None,
            storage_backend: None,
            max_isolates: 10,
            max_pinned_isolates_per_app,
            poll_interval_secs: 60,
            shutdown_timeout_secs: 0,
            blob_store: blob_store.clone(),
            workflow_blob_store: workflow_blob_store.clone(),
            max_step_blob_bytes: 64 * 1024 * 1024,
            workflow_advance_unsigned: true,
        });
        (app_id, blob_store, envs, logs, config, meter, blob_root)
    }

    async fn pg_client(db_url: &str) -> compio_postgres::Client {
        let (client, connection) = compio_postgres::connect(db_url, NoTls)
            .await
            .expect("connect workflow test pg");
        compio::runtime::spawn(async move {
            if let Err(e) = connection.run().await {
                tracing::error!(error = %e, "worker test pg connection error");
            }
        })
        .detach();
        client
    }

    async fn seed_unclaimed_workflow_run(
        db_url: &str,
        app_id: &Uuid,
        run_id: &str,
        workflow_name: &str,
        deploy_hash: &str,
    ) {
        let conn = pg_client(db_url).await;
        // A test seeds its app by INSERTing into `zeroship.apps` below, which
        // skips the migration apply that - in production - creates the app's
        // `app_<uuid>` workflow journal schema. `PgStore::provision` holds no
        // CREATE and cannot make that schema itself (2a44ea8ef), so it must
        // exist first; call the migration service's own provisioning
        // statement rather than a hand-rolled `CREATE SCHEMA`, so the journal
        // below ends up owned exactly the way a deployed app's is. Same
        // sequencing as `zeroship-plugin-db`'s and `zeroship-control`'s
        // workflow-journal test fixtures.
        provision_workflow_journal_schema(&conn, app_id)
            .await
            .expect("provision app workflow journal schema");
        PgStore::provision(&conn, app_id)
            .await
            .expect("provision worker workflow test journal");
        conn.execute(
            "INSERT INTO zeroship.plans \
                (id, name, base_fee_cents, included_units, spend_limit_default_cents, workflows_allowed, runtime_limits_json) \
             VALUES ($1, 'worker-workflow-test', 0, 1000000, 0, true, '{}'::json) \
             ON CONFLICT (id) DO UPDATE SET workflows_allowed = true, archived = false",
            &[&WORKFLOW_TEST_PLAN],
        )
        .await
        .expect("upsert worker workflow test plan");
        // The app name has to vary with the id. `apps.name` is UNIQUE, and every
        // caller seeds a fresh `Uuid::new_v4()`, so a fixed name means the
        // ON CONFLICT (id) arm never fires and the insert collides on
        // `apps_name_key` instead. These tests run concurrently, so a shared name
        // makes all but the first fail on contact.
        let app_name = format!("worker-workflow-test-app-{app_id}");
        // An app needs a project and a project needs an organization:
        // `apps.project_id` is NOT NULL against a RESTRICT foreign key. Workflow
        // admission is what these tests drive, not authority, so the
        // organization is left member-less.
        let organization_id = zeroship_core::typed_id::generate("org");
        let project_id = zeroship_core::typed_id::generate("prj");
        conn.execute(
            "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
             VALUES ($1, $2, 'Worker Workflow Fixture', 'fixture@zeroship.test')",
            &[&organization_id, &format!("worker-workflow-{}", Uuid::new_v4().simple())],
        )
        .await
        .expect("seed worker workflow test organization");
        conn.execute(
            "INSERT INTO zeroship.projects (id, organization_id, slug, name) \
             VALUES ($1, $2, 'default', 'Default')",
            &[&project_id, &organization_id],
        )
        .await
        .expect("seed worker workflow test project");
        conn.execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, workflows_enabled, project_id, organization_id) \
             SELECT $1, $3, $2, true, p.id, p.organization_id FROM zeroship.projects p WHERE p.id = $4 \
             ON CONFLICT (id) DO UPDATE SET plan_id = EXCLUDED.plan_id, workflows_enabled = true",
            &[app_id, &WORKFLOW_TEST_PLAN, &app_name, &project_id],
        )
        .await
        .expect("upsert worker workflow test app");
        // A deploy is identified by (app_id, deploy_hash), which is UNIQUE. Tests
        // deliberately seed several runs against one hash to exercise redeploy and
        // deploy pinning, so conflicting on `id` would miss and collide on
        // `app_deploys_app_id_deploy_hash_key` instead. Upsert on the real key and
        // take back whichever id won, so every run points at the row that exists.
        let deploy_id: String = conn
            .query_one(
                "INSERT INTO zeroship.app_deploys (id, app_id, deploy_hash, manifest_json, activated_at) \
                 VALUES ($1, $2, $3, '{}', now()) \
                 ON CONFLICT (app_id, deploy_hash) DO UPDATE SET activated_at = now() \
                 RETURNING id",
                &[&format!("dep_{app_id}_{run_id}"), app_id, &deploy_hash],
            )
            .await
            .expect("upsert worker workflow test deploy")
            .get(0);
        let tables = WorkflowTables::for_app_id(app_id);
        conn.execute(
            &format!(
                "INSERT INTO {} \
                    (id, workflow_name, app_id, deploy_id, state, input, started_at, wake_at) \
                 VALUES ($1, $2, $3, $4, 'queued', $5, now(), now())",
                tables.runs
            ),
            &[
                &run_id,
                &workflow_name,
                app_id,
                &deploy_id,
                &serde_json::json!({"orderId": "ord_1"}),
            ],
        )
        .await
        .expect("seed unclaimed workflow run");
    }

    async fn reclaim_workflow_run(db_url: &str, app_id: &Uuid, run_id: &str) {
        let conn = pg_client(db_url).await;
        let tables = WorkflowTables::for_app_id(app_id);
        conn.execute(
            &format!(
                "UPDATE {} \
                    SET state = 'running', claimed_by = NULL, dispatch_nonce = NULL, lease_expires = NULL, wake_at = now() \
                  WHERE id = $1",
                tables.runs
            ),
            &[&run_id],
        )
        .await
        .expect("reclaim workflow run for replay");
    }

    async fn steal_workflow_claim(db_url: &str, app_id: &Uuid, run_id: &str) {
        let conn = pg_client(db_url).await;
        let tables = WorkflowTables::for_app_id(app_id);
        let lease_expires_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_millis() as i64
            + 60_000;
        conn.execute(
            &format!(
                "UPDATE {} \
                    SET state = 'running', claimed_by = 'other-worker', dispatch_nonce = 'wfd_other', lease_expires = to_timestamp($2::double precision / 1000.0) \
                  WHERE id = $1",
                tables.runs
            ),
            &[&run_id, &(lease_expires_ms as f64)],
        )
        .await
        .expect("steal workflow claim");
    }

    async fn workflow_step_names(db_url: &str, app_id: &Uuid, run_id: &str) -> Vec<String> {
        let conn = pg_client(db_url).await;
        let tables = WorkflowTables::for_app_id(app_id);
        conn.query(
            &format!(
                "SELECT name FROM {} WHERE run_id = $1 ORDER BY ordinal",
                tables.steps
            ),
            &[&run_id],
        )
        .await
        .expect("load workflow step names")
        .into_iter()
        .map(|row| row.get("name"))
        .collect()
    }

    async fn workflow_step_output(
        db_url: &str,
        app_id: &Uuid,
        run_id: &str,
        name: &str,
    ) -> serde_json::Value {
        let conn = pg_client(db_url).await;
        let tables = WorkflowTables::for_app_id(app_id);
        conn.query_one(
            &format!(
                "SELECT output FROM {} WHERE run_id = $1 AND name = $2",
                tables.steps
            ),
            &[&run_id, &name],
        )
        .await
        .expect("load workflow step output")
        .get::<_, Option<serde_json::Value>>("output")
        .expect("inline workflow step output")
    }

    fn assert_workflow_ack(body: &[u8], run_id: &str) -> serde_json::Value {
        let result: serde_json::Value =
            serde_json::from_slice(body).expect("workflow advance ack JSON");
        assert_eq!(result["ack"], true, "workflow advance should ack: {result:?}");
        assert_eq!(result["runId"], run_id);
        assert!(
            result["registrations"]
                .as_array()
                .is_some_and(|registrations| registrations
                    .iter()
                    .any(|registration| registration["runId"] == run_id)),
            "ack registrations should include dispatched run: {result:?}"
        );
        result
    }

    fn assert_workflow_nack(
        body: &[u8],
        run_id: &str,
        kind: &str,
    ) -> serde_json::Value {
        let result: serde_json::Value =
            serde_json::from_slice(body).expect("workflow advance nack JSON");
        assert_eq!(result["nack"], true, "workflow advance should nack: {result:?}");
        assert_eq!(result["runId"], run_id);
        assert_eq!(result["nackKind"], kind);
        result
    }

    #[test]
    fn workflow_advance_first_frontier_returns_step_completed() {
        // The whole workspace runs on compio/io_uring (see AGENTS.md); a
        // machine that cannot create a compio runtime cannot run any of this
        // suite, so this fails the test rather than reporting a pass for
        // work it never did.
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        runtime.block_on(async {
            let db_url = workflow_test_db_url();
            let (app_id, blob_store, envs, logs, config, blob_root) = workflow_test_state(4);
            let deploy_hash = deploy_workflow_fixture(&blob_store, &app_id, "A").await;
            seed_unclaimed_workflow_run(
                &db_url,
                &app_id,
                "run_test",
                "Checkout",
                &deploy_hash,
            )
            .await;
            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .service(
                        web::resource("/workflow-advance-unsigned/{app_id}")
                            .route(web::post().to(workflow_advance_unsigned)),
                    ),
            )
            .await;

            let req = test::TestRequest::post()
                .uri(&format!("/workflow-advance-unsigned/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(serde_json::to_vec(&workflow_request(&app_id)).unwrap())
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            assert_workflow_ack(&body, "run_test");
            assert_eq!(
                workflow_step_names(&db_url, &app_id, "run_test").await,
                vec!["first".to_string()]
            );
            let output = workflow_step_output(&db_url, &app_id, "run_test", "first").await;
            assert_eq!(output["mark"], "A");
            assert_eq!(output["bodyRuns"], 1);

            let _ = std::fs::remove_dir_all(blob_root);
        });
    }

    #[test]
    fn workflow_advance_claim_lost_nacks_without_replay() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        runtime.block_on(async {
            let db_url = workflow_test_db_url();
            let (app_id, blob_store, envs, logs, config, blob_root) = workflow_test_state(4);
            let deploy_hash = deploy_workflow_fixture(&blob_store, &app_id, "CL").await;
            seed_unclaimed_workflow_run(
                &db_url,
                &app_id,
                "run_claim_lost",
                "Checkout",
                &deploy_hash,
            )
            .await;
            steal_workflow_claim(&db_url, &app_id, "run_claim_lost").await;
            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .service(
                        web::resource("/workflow-advance-unsigned/{app_id}")
                            .route(web::post().to(workflow_advance_unsigned)),
                    ),
            )
            .await;

            let req = test::TestRequest::post()
                .uri(&format!("/workflow-advance-unsigned/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(
                    serde_json::to_vec(&workflow_request_for_run(&app_id, "run_claim_lost"))
                        .unwrap(),
                )
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            assert_workflow_nack(&body, "run_claim_lost", "claimLost");
            assert!(
                workflow_step_names(&db_url, &app_id, "run_claim_lost")
                    .await
                    .is_empty(),
                "claim-lost dispatch must not replay or apply"
            );

            let _ = std::fs::remove_dir_all(blob_root);
        });
    }

    #[test]
    fn workflow_advance_concurrent_frontier_returns_outcomes_batch() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        runtime.block_on(async {
            let db_url = workflow_test_db_url();
            let (app_id, blob_store, envs, logs, config, blob_root) = workflow_test_state(4);
            let deploy_hash = deploy_workflow_fixture(&blob_store, &app_id, "C").await;
            seed_unclaimed_workflow_run(
                &db_url,
                &app_id,
                "run_test",
                "ConcurrentWorkflow",
                &deploy_hash,
            )
            .await;
            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .service(
                        web::resource("/workflow-advance-unsigned/{app_id}")
                            .route(web::post().to(workflow_advance_unsigned)),
                    ),
            )
            .await;

            let req = test::TestRequest::post()
                .uri(&format!("/workflow-advance-unsigned/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(
                    serde_json::to_vec(&workflow_request(&app_id))
                    .unwrap(),
                )
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            assert_workflow_ack(&body, "run_test");
            assert_eq!(
                workflow_step_names(&db_url, &app_id, "run_test").await,
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            );

            let _ = std::fs::remove_dir_all(blob_root);
        });
    }

    #[test]
    fn workflow_advance_feeds_platform_counters_and_workflow_steps_metric() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        runtime.block_on(async {
            let db_url = workflow_test_db_url();
            let (app_id, blob_store, envs, logs, config, meter, blob_root) =
                workflow_test_state_with_meter(4);
            let deploy_hash = deploy_workflow_fixture(&blob_store, &app_id, "M").await;
            seed_unclaimed_workflow_run(
                &db_url,
                &app_id,
                "run_test",
                "Checkout",
                &deploy_hash,
            )
            .await;
            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .service(
                        web::resource("/workflow-advance-unsigned/{app_id}")
                            .route(web::post().to(workflow_advance_unsigned)),
                    ),
            )
            .await;

            let payload = serde_json::to_vec(&workflow_request(&app_id)).unwrap();
            let req = test::TestRequest::post()
                .uri(&format!("/workflow-advance-unsigned/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(payload.clone())
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            assert_workflow_ack(&body, "run_test");

            let events = meter.drain();
            assert_eq!(
                usage_value(&events, app_id, "requests"),
                Some(1),
                "workflow advance is one metered request"
            );
            assert_eq!(
                usage_value(&events, app_id, "ingress_bytes"),
                Some(payload.len() as u64),
                "workflow advance ingress is the StepRequest JSON body"
            );
            assert_eq!(
                usage_value(&events, app_id, "egress_bytes"),
                Some(body.len() as u64),
                "workflow advance egress is the ack JSON body"
            );
            assert!(
                usage_value(&events, app_id, "wall_us").unwrap_or(0) > 0,
                "workflow advance records wall_us"
            );
            assert!(
                usage_value(&events, app_id, "cpu_us").unwrap_or(0) > 0,
                "workflow advance records cpu_us"
            );
            assert_eq!(
                usage_value(&events, app_id, "workflow_steps"),
                Some(1),
                "workflow advance records observability workflow_steps"
            );

            let _ = std::fs::remove_dir_all(blob_root);
        });
    }

    #[test]
    fn workflow_advance_replays_journal_hit_without_rerunning_body() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        runtime.block_on(async {
            let db_url = workflow_test_db_url();
            let (app_id, blob_store, envs, logs, config, blob_root) = workflow_test_state(4);
            let deploy_hash = deploy_workflow_fixture(&blob_store, &app_id, "A").await;
            seed_unclaimed_workflow_run(
                &db_url,
                &app_id,
                "run_test",
                "Checkout",
                &deploy_hash,
            )
            .await;
            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .service(
                        web::resource("/workflow-advance-unsigned/{app_id}")
                            .route(web::post().to(workflow_advance_unsigned)),
                    ),
            )
            .await;

            let first_req = test::TestRequest::post()
                .uri(&format!("/workflow-advance-unsigned/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(serde_json::to_vec(&workflow_request(&app_id)).unwrap())
                .to_request();
            let first_resp = test::call_service(&app, first_req).await;
            assert_eq!(first_resp.status(), StatusCode::OK);
            let first_body = test::read_body(first_resp).await;
            assert_workflow_ack(&first_body, "run_test");
            reclaim_workflow_run(&db_url, &app_id, "run_test").await;
            let second_req = test::TestRequest::post()
                .uri(&format!("/workflow-advance-unsigned/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(serde_json::to_vec(&workflow_request(&app_id)).unwrap())
                .to_request();
            let second_resp = test::call_service(&app, second_req).await;
            assert_eq!(second_resp.status(), StatusCode::OK);
            let second_body = test::read_body(second_resp).await;
            assert_workflow_ack(&second_body, "run_test");
            assert_eq!(
                workflow_step_names(&db_url, &app_id, "run_test").await,
                vec!["first".to_string(), "second".to_string()]
            );
            let second_output = workflow_step_output(&db_url, &app_id, "run_test", "second").await;
            assert_eq!(
                second_output["bodyRuns"], 2,
                "the completed first step must be replayed from journal, not re-run"
            );

            let _ = std::fs::remove_dir_all(blob_root);
        });
    }

    #[test]
    fn workflow_advance_keeps_in_flight_run_on_pinned_deploy_after_redeploy() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        runtime.block_on(async {
            let db_url = workflow_test_db_url();
            let (app_id, blob_store, envs, logs, config, blob_root) = workflow_test_state(4);
            let deploy_a = deploy_workflow_fixture(&blob_store, &app_id, "A").await;
            let deploy_b = deploy_workflow_fixture(&blob_store, &app_id, "B").await;
            assert_ne!(deploy_a, deploy_b);
            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .service(
                        web::resource("/workflow-advance-unsigned/{app_id}")
                            .route(web::post().to(workflow_advance_unsigned)),
                    ),
            )
            .await;

            for (idx, (deploy_hash, expected_mark)) in
                [(&deploy_a, "A"), (&deploy_b, "B"), (&deploy_a, "A")]
                    .into_iter()
                    .enumerate()
            {
                let run_id = format!("run_test_pinned_{idx}");
                seed_unclaimed_workflow_run(&db_url, &app_id, &run_id, "Checkout", deploy_hash).await;
                let req = test::TestRequest::post()
                    .uri(&format!("/workflow-advance-unsigned/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                    .set_payload(serde_json::to_vec(&workflow_request_for_run(&app_id, &run_id)).unwrap())
                    .to_request();
                let resp = test::call_service(&app, req).await;
                assert_eq!(resp.status(), StatusCode::OK);
                let body = test::read_body(resp).await;
                assert_workflow_ack(&body, &run_id);
                let output = workflow_step_output(&db_url, &app_id, &run_id, "first").await;
                assert_eq!(output["mark"], expected_mark);
            }
            assert!(crate::cache::has_pinned_workflow_app(&app_id, &deploy_a));
            assert!(crate::cache::has_pinned_workflow_app(&app_id, &deploy_b));

            let _ = std::fs::remove_dir_all(blob_root);
        });
    }

    #[test]
    fn workflow_advance_pinned_isolate_budget_lru_evicts_per_app() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        runtime.block_on(async {
            let db_url = workflow_test_db_url();
            let (app_id, blob_store, envs, logs, config, blob_root) = workflow_test_state(1);
            let deploy_a = deploy_workflow_fixture(&blob_store, &app_id, "A").await;
            let deploy_b = deploy_workflow_fixture(&blob_store, &app_id, "B").await;
            let app = test::init_service(
                web::App::new()
                    .state(config)
                    .state(envs)
                    .state(logs)
                    .service(
                        web::resource("/workflow-advance-unsigned/{app_id}")
                            .route(web::post().to(workflow_advance_unsigned)),
                    ),
            )
            .await;

            seed_unclaimed_workflow_run(
                &db_url,
                &app_id,
                "run_test_lru_a",
                "Checkout",
                &deploy_a,
            )
            .await;
            let req_a = test::TestRequest::post()
                .uri(&format!("/workflow-advance-unsigned/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(
                    serde_json::to_vec(&workflow_request_for_run(&app_id, "run_test_lru_a"))
                    .unwrap(),
                )
                .to_request();
            let resp_a = test::call_service(&app, req_a).await;
            assert_eq!(resp_a.status(), StatusCode::OK);
            assert!(crate::cache::has_pinned_workflow_app(&app_id, &deploy_a));

            seed_unclaimed_workflow_run(
                &db_url,
                &app_id,
                "run_test_lru_b",
                "Checkout",
                &deploy_b,
            )
            .await;
            let req_b = test::TestRequest::post()
                .uri(&format!("/workflow-advance-unsigned/{}", worker_app_path(&app_id)))
                .header("authorization", gateway_authorization())
                .set_payload(
                    serde_json::to_vec(&workflow_request_for_run(&app_id, "run_test_lru_b"))
                    .unwrap(),
                )
                .to_request();
            let resp_b = test::call_service(&app, req_b).await;
            assert_eq!(resp_b.status(), StatusCode::OK);
            assert!(!crate::cache::has_pinned_workflow_app(&app_id, &deploy_a));
            assert!(crate::cache::has_pinned_workflow_app(&app_id, &deploy_b));

            let _ = std::fs::remove_dir_all(blob_root);
        });
    }
}
