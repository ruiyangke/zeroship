//! The N-API transport (design §B.3 + §C.4 + §C.5) — compiled only with the `napi`
//! feature.
//!
//! ## Sync, DB-free entrypoints (run inline, no bridge)
//! `irVersion`, `loadVerify` — pure functions ([`crate::api`]) returning a JSON
//! string on the napi call thread.
//!
//! ## Async, host-driven entrypoints (fire-and-resolve)
//! `apply`, `status`, `history` — each:
//! 1. builds a [`TsfnDispatch`] from the JS host-driver callback (a
//!    `ThreadsafeFunction`);
//! 2. `env.create_deferred()`s a JS `Promise`;
//! 3. spawns the engine on its OWN std::thread ([`crate::runtime::run_engine_blocking`])
//!    running a reactor-less `futures::executor::block_on`;
//! 4. resolves/rejects the `Promise` cross-thread from that worker thread when
//!    `block_on` completes.
//!
//! The JS thread is **never** `join()`ed on the worker — that would deadlock
//! libuv/Bun (the host-driver TSFN callback can't run while the JS thread is parked
//! in the napi call). This is the fire-and-resolve topology (§B.5).
//!
//! ## The tokio-free verb bridge (§B.3)
//! Each `PgSession` verb ([`crate::session::NapiHostSession`]) calls
//! [`TsfnDispatch::dispatch`], which: allocates a `futures::channel::oneshot`, moves
//! the `Sender` into the TSFN payload, and `tsfn.call(..., Blocking)`. On the JS
//! thread the TSFN's `build_callback` closure marshals the request to a JS object,
//! builds a per-call `done(err, reply)` JS function that captures the `Sender`, and
//! invokes the host driver. napi-rs delivers the `(request, done)` tuple as a SINGLE
//! JS array arg, so the **host-driver JS contract is
//! `hostDriver([request, done]) => void`** (destructure the array). The host does
//! its real async DB work and calls `done(err, reply)`; `done`'s Rust body fires the
//! `Sender`, waking the parked `block_on`. NO `#[napi] async fn`, NO `Promise::await`,
//! NO `tokio_rt`.

use std::cell::RefCell;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::Env;
use napi_derive::napi;

use zeroship_migrate::approval::Approval;
use zeroship_migrate::conn::ExecutorConfig;
use zeroship_migrate::model::migration::Migration;

use crate::api;
use crate::marshal::{JsError, JsReply, JsRequest};
use crate::runtime::run_engine_blocking;
use crate::session::{NapiHostSession, VerbDispatch, VerbReply};

// ---------------------------------------------------------------------------
// Sync, DB-free entrypoints (§C.5) — inline on the napi call thread.
// ---------------------------------------------------------------------------

/// The IR-format version this addon was built against (§5.3 fail-closed floor).
#[napi(js_name = "irVersion")]
pub fn ir_version() -> u32 {
    api::current_ir_version()
}

/// Load + verify an IR document (the sync, DB-free deploy gate, §C.5). Returns a
/// JSON-serialized `LoadVerifyReport` (`{ ok, ir_version, op_count, error }`); never
/// throws for a malformed document.
#[napi(js_name = "loadVerify")]
pub fn load_verify(
    ir_json: String,
    deploying_app: String,
    dialect: String,
    registry_json: String,
) -> String {
    api::load_verify_json(&ir_json, &deploying_app, &dialect, &registry_json)
}

// ---------------------------------------------------------------------------
// The host-driver TSFN payload + dispatch (§B.3).
// ---------------------------------------------------------------------------

/// What crosses to the JS thread per verb: the request + the oneshot `Sender` the
/// `done` callback fires. Both are `Send + 'static` (owned data + a
/// `oneshot::Sender<VerbReply>`), so no `!Send` engine state crosses (§B.3).
struct VerbCall {
    request: JsRequest,
    reply: futures::channel::oneshot::Sender<VerbReply>,
}

/// The `done(err, reply)` JS callback the host driver invokes to complete a verb.
/// `'static` because it is stored in the TSFN payload / handed to the host across
/// call boundaries (detached from the per-call `Env` via [`detach_function`]).
type DoneFn = Function<'static, (Option<JsError>, Option<JsReply>), ()>;

/// The JS host-driver function: `hostDriver(request, done) => void`. `'static` so it
/// can be consumed into the `ThreadsafeFunction`.
type HostDriverFn = Function<'static, (JsRequest, DoneFn), ()>;

/// The TSFN's `CallJsBackArgs`: the JS host driver is invoked as
/// `hostDriver(request, done)`.
type HostTsfn = ThreadsafeFunction<
    VerbCall, // T — payload crossing to the JS thread
    (),       // Return — the host callback returns void; it calls `done` instead
    (JsRequest, DoneFn), // CallJsBackArgs
    Status,
    false, // CalleeHandled = false: we surface driver errors via `done(err, …)`
>;

/// A [`VerbDispatch`] that fires the host-driver `ThreadsafeFunction` and parks on a
/// oneshot the JS `done` callback fires (§B.3).
pub struct TsfnDispatch {
    tsfn: HostTsfn,
}

impl VerbDispatch for TsfnDispatch {
    async fn dispatch(&self, req: JsRequest) -> VerbReply {
        let (tx, rx) = futures::channel::oneshot::channel::<VerbReply>();
        let status = self.tsfn.call(
            VerbCall {
                request: req,
                reply: tx,
            },
            ThreadsafeFunctionCallMode::Blocking,
        );
        if status != Status::Ok {
            return Err(JsError {
                message: format!("host driver TSFN call failed: {status:?}"),
                code: None,
            });
        }
        // Park the (single-threaded) engine future on the oneshot the `done`
        // callback fires from the JS thread. Awaiting a `oneshot::Receiver` — NEVER
        // a JS Promise — keeps the reactor-less block_on sufficient (§C.4).
        rx.await.unwrap_or(Err(JsError {
            message: "host driver dropped the `done` callback without replying".to_string(),
            code: None,
        }))
    }
}

/// Build a [`TsfnDispatch`] from the JS host-driver function `(request, done) => void`.
///
/// The `build_callback` closure runs on the JS main thread: it converts the
/// [`JsRequest`] to a JS object and builds a per-call `done(err, reply)` JS function
/// (via [`Env::create_function_from_closure`]) that captures the oneshot `Sender`,
/// then returns `(request, done)` for the host driver call.
fn build_host_dispatch(host_driver: HostDriverFn) -> Result<TsfnDispatch> {
    let tsfn: HostTsfn = host_driver
        .build_threadsafe_function::<VerbCall>()
        .callee_handled::<false>()
        .build_callback(|ctx| {
            // On the JS thread: `ctx.value` is the VerbCall payload.
            let VerbCall { request, reply } = ctx.value;
            let env = ctx.env;

            // A per-call `done(err, reply)` JS function that fires the oneshot.
            // The Sender is single-use: a RefCell<Option<_>> lets the `Fn` closure
            // take it on the first `done` call. Subsequent calls are ignored (the
            // host contract is exactly-once, and a leaked receiver would already
            // have resolved).
            let sender_cell = RefCell::new(Some(reply));
            let done_local = env.create_function_from_closure(
                "done",
                move |cb_ctx: FunctionCallContext| -> Result<()> {
                    let err: Option<JsError> = cb_ctx.get::<Option<JsError>>(0).unwrap_or(None);
                    let ok: Option<JsReply> = cb_ctx.get::<Option<JsReply>>(1).unwrap_or(None);
                    let outcome: VerbReply = match (err, ok) {
                        (Some(e), _) => Err(e),
                        (None, Some(reply)) => Ok(reply),
                        (None, None) => Err(JsError {
                            message: "host `done` called with neither error nor reply".to_string(),
                            code: None,
                        }),
                    };
                    if let Some(tx) = sender_cell.borrow_mut().take() {
                        // Fire the oneshot — this is the cross-thread Waker::wake
                        // that unparks the engine worker's block_on (§B.3). A
                        // dropped receiver (engine gone) is not an error here.
                        let _ = tx.send(outcome);
                    }
                    Ok(())
                },
            )?;
            // Detach `done`'s borrow of the per-call `Env` so it satisfies the
            // `'static` `CallJsBackArgs` bound (the underlying napi_value outlives
            // the call — it is passed to the host driver).
            let done = detach_function::<(Option<JsError>, Option<JsReply>), ()>(env, done_local)?;

            Ok((request, done))
        })?;
    Ok(TsfnDispatch { tsfn })
}

/// Detach a `Function<'_>`'s borrow of a per-call `Env` into a `Function<'static>`
/// via a raw napi_value round-trip. Sound because the `napi_value` outlives the
/// native call boundary (it is handed to JS / stored in the TSFN payload) — the same
/// round-trip napi's own return codegen performs.
fn detach_function<Args, Ret>(
    env: Env,
    f: Function<'_, Args, Ret>,
) -> Result<Function<'static, Args, Ret>>
where
    Args: JsValuesTupleIntoVec + FromNapiValue + 'static,
    Ret: FromNapiValue + 'static,
{
    let raw = unsafe { <Function<Args, Ret> as ToNapiValue>::to_napi_value(env.raw(), f)? };
    let detached =
        unsafe { <Function<'static, Args, Ret> as FromNapiValue>::from_napi_value(env.raw(), raw)? };
    Ok(detached)
}

// ---------------------------------------------------------------------------
// Async, host-driven entrypoints (§C.5) — fire-and-resolve.
// ---------------------------------------------------------------------------

/// The JSON-serialized outcome an async entrypoint resolves its Promise with. Plain
/// owned data, `Send + 'static`, so it crosses from the worker thread to the JS
/// thread via `deferred.resolve` (§B.3).
type EngineOutcome = std::result::Result<String, String>;

/// Detach the borrow of a `create_deferred` promise `Object<'_>` (which borrows the
/// by-value `Env` local) so it can be returned from a `#[napi]` fn. The underlying
/// `napi_value` outlives the native call (it is handed to JS), so re-wrapping it as
/// an owned `Object<'static>` via the raw round-trip is sound — it is the same
/// `napi_value` napi's own return codegen would forward.
fn detach_promise(env: Env, promise: Object<'_>) -> Result<Object<'static>> {
    let raw = unsafe { <Object as ToNapiValue>::to_napi_value(env.raw(), promise)? };
    let detached = unsafe { <Object<'static> as FromNapiValue>::from_napi_value(env.raw(), raw)? };
    Ok(detached)
}

/// `apply` — drive the engine's generic `executor::apply` over the host driver
/// (§C.5 convergence point). Migrations cross as a JSON `Vec<Migration>`.
///
/// Returns a `Promise<string>` resolving to a JSON `ApplyOutcome`
/// (`{ applied, skipped, recovered }`) or rejecting with the engine error.
#[napi(ts_return_type = "Promise<string>")]
pub fn apply(
    env: Env,
    #[napi(ts_arg_type = "(args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void]) => void")]
    host_driver: HostDriverFn,
    project_id: String,
    project_schema: String,
    migrator_role: Option<String>,
    migrations_json: String,
    approved: bool,
    applied_by: String,
) -> Result<Object<'static>> {
    let migrations: Vec<Migration> = serde_json::from_str(&migrations_json)
        .map_err(|e| Error::from_reason(format!("migrations_json is not Vec<Migration>: {e}")))?;

    let dispatch = build_host_dispatch(host_driver)?;

    let (deferred, promise) = env.create_deferred::<String, _>()?;

    let approval = if approved { Approval::Approved } else { Approval::None };

    run_engine_blocking(
        move || async move {
            // The engine future — strictly one-verb-at-a-time over the host session.
            let session = NapiHostSession::new(dispatch);
            let mut cfg = ExecutorConfig::new(project_id, project_schema);
            if let Some(role) = migrator_role {
                cfg = cfg.with_migrator_role(role);
            }
            let out = zeroship_migrate::apply::executor::apply(
                &session, &cfg, &migrations, approval, &applied_by,
            )
            .await;
            match out {
                Ok(outcome) => Ok(serde_json::json!({
                    "applied": outcome.applied,
                    "skipped": outcome.skipped,
                    "recovered": outcome.recovered,
                })
                .to_string()),
                Err(e) => Err(e.to_string()),
            }
        },
        move |outcome: EngineOutcome| match outcome {
            Ok(json) => deferred.resolve(move |_| Ok(json)),
            Err(msg) => deferred.reject(Error::from_reason(msg)),
        },
    );

    detach_promise(env, promise)
}

/// `applyIr` — the HOST-AUTHORING apply entry (§D.1): take a pure-JS `.ir.json`
/// ENVELOPE (`{ ir_version, name, ops }`), run the fail-closed LOAD GATE + LOWER
/// **in Rust** (stamping `owner_app` + folding the authoritative `Checksum::of_ir`
/// — the checksum is NEVER computed in JS), then drive `executor::apply` over the
/// host driver. The envelope must NOT carry `owner_app`; it is stamped from the
/// `owner_app` arg (provenance, §D.1).
///
/// This is the entry the `@zeroship/migrate/host` facade's `apply` calls: the host
/// recorder produces the envelope purely in JS, this addon owns the checksum.
/// Returns a `Promise<string>` resolving to a JSON `ApplyOutcome`.
#[napi(ts_return_type = "Promise<string>")]
pub fn apply_ir(
    env: Env,
    #[napi(ts_arg_type = "(args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void]) => void")]
    host_driver: HostDriverFn,
    owner_app: String,
    project_schema: String,
    migrator_role: Option<String>,
    dialect: String,
    registry_json: String,
    envelope_json: String,
    approved: bool,
    applied_by: String,
) -> Result<Object<'static>> {
    // Lower the envelope → Vec<Migration> IN RUST (checksum folded here, §D.1).
    let migrations: Vec<Migration> = {
        let json = crate::lower::lower_envelope_to_migrations_json(
            &envelope_json,
            &owner_app,
            &project_schema,
            &dialect,
            &registry_json,
        )
        .map_err(Error::from_reason)?;
        serde_json::from_str(&json)
            .map_err(|e| Error::from_reason(format!("lowered migrations re-parse failed: {e}")))?
    };

    let dispatch = build_host_dispatch(host_driver)?;
    let (deferred, promise) = env.create_deferred::<String, _>()?;
    let approval = if approved { Approval::Approved } else { Approval::None };

    run_engine_blocking(
        move || async move {
            let session = NapiHostSession::new(dispatch);
            let mut cfg = ExecutorConfig::new(owner_app_project(&project_schema), project_schema);
            if let Some(role) = migrator_role {
                cfg = cfg.with_migrator_role(role);
            }
            let out = zeroship_migrate::apply::executor::apply(
                &session, &cfg, &migrations, approval, &applied_by,
            )
            .await;
            match out {
                Ok(outcome) => Ok(serde_json::json!({
                    "applied": outcome.applied,
                    "skipped": outcome.skipped,
                    "recovered": outcome.recovered,
                })
                .to_string()),
                Err(e) => Err(e.to_string()),
            }
        },
        move |outcome: EngineOutcome| match outcome {
            Ok(json) => deferred.resolve(move |_| Ok(json)),
            Err(msg) => deferred.reject(Error::from_reason(msg)),
        },
    );

    detach_promise(env, promise)
}

/// The `project_id` an `ExecutorConfig` carries. The IR host path uses the project
/// schema as the project id (a fresh single-app project's schema == its id in the
/// create-first posture), matching the native oracle's `ExecutorConfig::new(schema,
/// schema)` (`build_new_generate_pg.rs`). A distinct project id can be threaded
/// through a future facade arg.
fn owner_app_project(project_schema: &str) -> String {
    project_schema.to_string()
}

/// `status` — the generic `ops::status::status` over the host driver (§C.5).
/// Migrations cross as a JSON `Vec<Migration>`. Resolves to a JSON `MigrationStatus`.
#[napi(ts_return_type = "Promise<string>")]
pub fn status(
    env: Env,
    #[napi(ts_arg_type = "(args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void]) => void")]
    host_driver: HostDriverFn,
    project_id: String,
    project_schema: String,
    migrations_json: String,
) -> Result<Object<'static>> {
    let migrations: Vec<Migration> = serde_json::from_str(&migrations_json)
        .map_err(|e| Error::from_reason(format!("migrations_json is not Vec<Migration>: {e}")))?;
    let dispatch = build_host_dispatch(host_driver)?;
    let (deferred, promise) = env.create_deferred::<String, _>()?;

    run_engine_blocking(
        move || async move {
            let session = NapiHostSession::new(dispatch);
            let cfg = ExecutorConfig::new(project_id, project_schema);
            match zeroship_migrate::ops::status::status(&session, &cfg, &migrations).await {
                // `MigrationStatus` is not `Serialize`; project the load-bearing
                // fields (current version + applied/pending version ids) to JSON.
                Ok(s) => Ok(serde_json::json!({
                    "current_version": s.current_version.as_ref().map(|v| v.as_str().to_string()),
                    "applied": s.applied.iter().map(|e| e.version.clone()).collect::<Vec<_>>(),
                    "pending": s.pending.iter().map(|v| v.as_str().to_string()).collect::<Vec<_>>(),
                    "rolled_back": s.rolled_back.iter().map(|e| e.version.clone()).collect::<Vec<_>>(),
                })
                .to_string()),
                Err(e) => Err(e.to_string()),
            }
        },
        move |outcome: EngineOutcome| match outcome {
            Ok(json) => deferred.resolve(move |_| Ok(json)),
            Err(msg) => deferred.reject(Error::from_reason(msg)),
        },
    );

    detach_promise(env, promise)
}

/// `history` — the generic `ops::status::history` over the host driver (§C.5).
/// Resolves to a JSON `Vec<HistoryEvent>`.
#[napi(ts_return_type = "Promise<string>")]
pub fn history(
    env: Env,
    #[napi(ts_arg_type = "(args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void]) => void")]
    host_driver: HostDriverFn,
    project_id: String,
    project_schema: String,
) -> Result<Object<'static>> {
    let dispatch = build_host_dispatch(host_driver)?;
    let (deferred, promise) = env.create_deferred::<String, _>()?;

    run_engine_blocking(
        move || async move {
            let session = NapiHostSession::new(dispatch);
            let cfg = ExecutorConfig::new(project_id, project_schema);
            match zeroship_migrate::ops::status::history(&session, &cfg).await {
                // `HistoryEvent` is not `Serialize`; project the audit fields.
                Ok(h) => Ok(serde_json::Value::Array(
                    h.iter()
                        .map(|e| {
                            serde_json::json!({
                                "event_seq": e.event_seq,
                                "version": e.version,
                                "name": e.name,
                                "kind": match e.kind {
                                    zeroship_migrate::apply::journal::HistoryKind::Completed => "applied",
                                    zeroship_migrate::apply::journal::HistoryKind::RolledBack => "rolled_back",
                                },
                                "at": e.at,
                                "applied_by": e.applied_by,
                                "checksum": e.checksum,
                            })
                        })
                        .collect(),
                )
                .to_string()),
                Err(e) => Err(e.to_string()),
            }
        },
        move |outcome: EngineOutcome| match outcome {
            Ok(json) => deferred.resolve(move |_| Ok(json)),
            Err(msg) => deferred.reject(Error::from_reason(msg)),
        },
    );

    detach_promise(env, promise)
}
