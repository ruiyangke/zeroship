//! The N-API transport (design §B.3 + §C.4 + §C.5) — compiled only with the `napi`
//! feature.
//!
//! ## Sync, DB-free entrypoints (run inline, no bridge)
//! `irVersion`, `loadVerify` — pure functions ([`crate::api`]). `loadVerify` returns
//! a typed [`LoadVerifyReply`] on the napi call thread (no JSON string).
//!
//! ## Async, host-driven entrypoints (fire-and-resolve)
//! `applyIr`, `apply`, `status`, `history` — each goes through the ONE generic
//! [`run_verb`]: it builds a [`TsfnDispatch`] from the JS host-driver callback, opens
//! a `create_deferred` promise, spawns the engine on its OWN std::thread
//! ([`crate::runtime::run_engine_blocking`]) running a reactor-less
//! `futures::executor::block_on`, and resolves/rejects the promise cross-thread with
//! a TYPED reply (`ApplyReply`/`StatusReply`/`HistoryReply`) when `block_on`
//! completes — NO `Promise<string>`, NO per-verb copy-pasted plumbing.
//!
//! The JS thread is **never** `join()`ed on the worker — that would deadlock
//! libuv/Bun (the host-driver TSFN callback can't run while the JS thread is parked
//! in the napi call). This is the fire-and-resolve topology (§B.5).
//!
//! ## The tokio-free verb bridge (§B.3)
//! Each `SqlSession` verb ([`crate::session::NapiHostSession`]) calls
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
use std::future::Future;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::Env;
use napi_derive::napi;

use zero_migrate::apply::executor::ApplyOutcome;
use zero_migrate::apply::journal::{HistoryEvent, HistoryKind};
use zero_migrate::approval::Approval;
use zero_migrate::conn::ExecutorConfig;
use zero_migrate::model::migration::Migration;
use zero_migrate::ops::status::MigrationStatus;

use crate::api;
use crate::marshal::{JsError, JsReply, JsRequest};
use crate::runtime::run_engine_blocking;
use crate::session::{NapiHostSession, VerbDispatch, VerbReply};
use crate::wire::{
    ApplyReply, ApplyRequest, HistoryEventDto, HistoryReply, HistoryRequest, LoadVerifyReply,
    StatusReply, StatusRequest,
};

/// The dialect a host-driven `apply` targets over the `SqlSession` seam. Only the
/// two NETWORK dialects reach the host driver — SQLite is in-process rusqlite and
/// never crosses the seam, so it is not a host-apply target.
#[derive(Debug, Clone, Copy)]
enum ApplyDialect {
    Postgres,
    Mysql,
}

impl ApplyDialect {
    /// Map the wire dialect spelling to the host-apply backend selector. `"sqlite"`
    /// is rejected here: it has no host-driver path (in-process rusqlite).
    fn parse(s: &str) -> std::result::Result<Self, String> {
        match s {
            "postgres" => Ok(Self::Postgres),
            "mysql" => Ok(Self::Mysql),
            "sqlite" => Err(
                "sqlite has no host-driver apply path (it runs in-process via rusqlite); \
                 pass a postgres or mysql driver"
                    .to_string(),
            ),
            other => Err(format!(
                "unknown dialect {other:?} (expected postgres|mysql for host apply)"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Sync, DB-free entrypoints (§C.5) — inline on the napi call thread.
// ---------------------------------------------------------------------------

/// The IR-format version this addon was built against (§5.3 fail-closed floor).
#[napi(js_name = "irVersion")]
pub fn ir_version() -> u32 {
    api::current_ir_version()
}

/// Load + verify an IR document (the sync, DB-free deploy gate, §C.5). Returns a
/// typed [`LoadVerifyReply`]; never throws for a malformed document.
#[napi(js_name = "loadVerify")]
pub fn load_verify(
    ir_json: String,
    deploying_app: String,
    dialect: String,
    registry: std::collections::HashMap<String, String>,
) -> LoadVerifyReply {
    api::load_verify(&ir_json, &deploying_app, &dialect, &registry)
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
// Async, host-driven entrypoints (§C.5) — the ONE generic driver.
// ---------------------------------------------------------------------------

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

/// The ONE generic host-verb driver (redesign step 5a) — the single home of the
/// build-dispatch → create-deferred → spawn-engine → resolve/reject plumbing every
/// async verb shares.
///
/// - `T` is the verb's TYPED reply (`ApplyReply`/`StatusReply`/`HistoryReply`) — it
///   crosses to JS via `deferred.resolve`, so it must be `ToNapiValue + Send`.
/// - `engine` runs on the worker thread; it is handed the host [`NapiHostSession`]
///   and returns `Result<T, String>` (the projected typed reply, or an engine-error
///   message that becomes a promise rejection).
///
/// The returned `Object` is the JS `Promise<T>`; the engine's whole lifetime lives on
/// the worker thread (the JS thread is never joined — §B.5).
fn run_verb<T, Fut, E>(env: Env, host_driver: HostDriverFn, engine: E) -> Result<Object<'static>>
where
    T: ToNapiValue + Send + 'static,
    Fut: Future<Output = std::result::Result<T, String>>,
    E: FnOnce(NapiHostSession<TsfnDispatch>) -> Fut + Send + 'static,
{
    let dispatch = build_host_dispatch(host_driver)?;
    let (deferred, promise) = env.create_deferred::<T, _>()?;

    run_engine_blocking(
        move || {
            let session = NapiHostSession::new(dispatch);
            engine(session)
        },
        move |outcome: std::result::Result<T, String>| match outcome {
            Ok(reply) => deferred.resolve(move |_| Ok(reply)),
            Err(msg) => deferred.reject(Error::from_reason(msg)),
        },
    );

    detach_promise(env, promise)
}

// ---------------------------------------------------------------------------
// Engine-result → typed-reply projections (named — no closure-local mapping).
// ---------------------------------------------------------------------------

/// Project an [`ApplyOutcome`] into the typed [`ApplyReply`].
fn apply_reply(outcome: ApplyOutcome) -> ApplyReply {
    ApplyReply {
        applied: outcome.applied,
        skipped: outcome.skipped,
        recovered: outcome.recovered,
    }
}

/// Project a [`MigrationStatus`] into the typed [`StatusReply`] (the load-bearing
/// fields: current version + applied/pending/rolled-back version ids).
fn status_reply(s: &MigrationStatus) -> StatusReply {
    StatusReply {
        current_version: s.current_version.as_ref().map(|v| v.as_str().to_string()),
        applied: s.applied.iter().map(|e| e.version.clone()).collect(),
        pending: s.pending.iter().map(|v| v.as_str().to_string()).collect(),
        rolled_back: s.rolled_back.iter().map(|e| e.version.clone()).collect(),
    }
}

/// The wire spelling of a [`HistoryKind`] — the single home of the mapping (was a
/// closure-local `match` in the `history` entrypoint).
fn history_kind_str(kind: HistoryKind) -> &'static str {
    match kind {
        HistoryKind::Completed => "applied",
        HistoryKind::RolledBack => "rolled_back",
    }
}

/// Project one [`HistoryEvent`] into the typed [`HistoryEventDto`] (`event_seq` as a
/// JS `bigint`, napi6).
fn history_event_dto(e: &HistoryEvent) -> HistoryEventDto {
    HistoryEventDto {
        event_seq: e.event_seq.into(),
        version: e.version.clone(),
        name: e.name.clone(),
        kind: history_kind_str(e.kind).to_string(),
        at: e.at.clone(),
        applied_by: e.applied_by.clone(),
        checksum: e.checksum.clone(),
    }
}

/// Project a `Vec<HistoryEvent>` into the typed [`HistoryReply`].
fn history_reply(events: &[HistoryEvent]) -> HistoryReply {
    HistoryReply {
        events: events.iter().map(history_event_dto).collect(),
    }
}

// ---------------------------------------------------------------------------
// The typed verbs (§C.5) — each is a thin `run_verb` closure over the engine.
// ---------------------------------------------------------------------------

/// `applyIr` — the HOST-AUTHORING apply entry (§D.1): take a pure-JS `.ir.json`
/// ENVELOPE (`{ ir_version, name, ops }`) as a typed [`ApplyRequest`], run the
/// fail-closed LOAD GATE + LOWER **in Rust** (stamping `owner_app` + folding the
/// authoritative `Checksum::of_ir` — the checksum is NEVER computed in JS), then
/// drive `executor::apply` over the host driver. The envelope must NOT carry
/// `owner_app`; it is stamped from `req.owner_app` (provenance, §D.1).
///
/// This is the entry the `zero-migrate-engine` facade's `apply` calls: the pure-JS
/// recorder produces the envelope, this addon owns the checksum.
/// Resolves to a typed [`ApplyReply`].
#[napi(ts_return_type = "Promise<ApplyReply>")]
pub fn apply_ir(
    env: Env,
    #[napi(ts_arg_type = "(args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void]) => void")]
    host_driver: HostDriverFn,
    req: ApplyRequest,
) -> Result<Object<'static>> {
    // Lower the envelope → Vec<Migration> IN RUST (checksum folded here, §D.1). The
    // `ops` AST crossed as a real JS value; re-serialize it for the lower gate.
    let envelope_json = serde_json::to_string(&req.envelope)
        .map_err(|e| Error::from_reason(format!("envelope is not serializable: {e}")))?;
    let registry_json = serde_json::to_string(&req.registry)
        .map_err(|e| Error::from_reason(format!("registry is not serializable: {e}")))?;
    let migrations: Vec<Migration> = {
        let json = crate::lower::lower_envelope_to_migrations_json(
            &envelope_json,
            &req.owner_app,
            &req.project_schema,
            &req.dialect,
            &registry_json,
        )
        .map_err(Error::from_reason)?;
        serde_json::from_str(&json)
            .map_err(|e| Error::from_reason(format!("lowered migrations re-parse failed: {e}")))?
    };

    // Dialect selects the backend (C1 fix, step 4e): Postgres and MySQL ride the
    // SAME `SqlSession` seam, but each dialect's lock / journal / placeholder SQL
    // lives in its own `MigrationBackend`. `apply` builds `PostgresBackend`;
    // `apply_with_lock_mysql` builds `MysqlBackend` (`GET_LOCK`, MySQL journal DDL,
    // `?` placeholders). SQLite is in-process rusqlite and never reaches the host
    // seam, so it is not a valid host-driver dialect here.
    let target = ApplyDialect::parse(&req.dialect).map_err(Error::from_reason)?;

    let ApplyRequest {
        owner_app: _,
        project_schema,
        migrator_role,
        approved,
        applied_by,
        ..
    } = req;
    let approval = if approved { Approval::Approved } else { Approval::None };

    run_verb(env, host_driver, move |session| async move {
        let mut cfg = ExecutorConfig::new(owner_app_project(&project_schema), project_schema);
        if let Some(role) = migrator_role {
            cfg = cfg.with_migrator_role(role);
        }
        let out = match target {
            ApplyDialect::Postgres => {
                zero_migrate::apply::executor::apply(
                    &session, &cfg, &migrations, approval, &applied_by,
                )
                .await
            }
            ApplyDialect::Mysql => {
                zero_migrate::apply::executor::apply_with_lock_mysql(
                    &session,
                    &cfg,
                    &migrations,
                    approval,
                    &applied_by,
                    zero_migrate::apply::executor::LockMode::Acquire,
                )
                .await
            }
        };
        out.map(apply_reply).map_err(|e| e.to_string())
    })
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
/// Migrations cross as a typed `Vec<JsonValue>` (each a `Migration`). Resolves to a
/// typed [`StatusReply`].
#[napi(ts_return_type = "Promise<StatusReply>")]
pub fn status(
    env: Env,
    #[napi(ts_arg_type = "(args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void]) => void")]
    host_driver: HostDriverFn,
    req: StatusRequest,
) -> Result<Object<'static>> {
    let migrations: Vec<Migration> = req
        .migrations
        .iter()
        .map(|v| serde_json::from_value(v.clone()))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::from_reason(format!("a status migration is not a Migration: {e}")))?;
    let StatusRequest {
        project_id,
        project_schema,
        ..
    } = req;

    run_verb(env, host_driver, move |session| async move {
        let cfg = ExecutorConfig::new(project_id, project_schema);
        zero_migrate::ops::status::status(&session, &cfg, &migrations)
            .await
            .map(|s| status_reply(&s))
            .map_err(|e| e.to_string())
    })
}

/// `history` — the generic `ops::status::history` over the host driver (§C.5).
/// Resolves to a typed [`HistoryReply`].
#[napi(ts_return_type = "Promise<HistoryReply>")]
pub fn history(
    env: Env,
    #[napi(ts_arg_type = "(args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void]) => void")]
    host_driver: HostDriverFn,
    req: HistoryRequest,
) -> Result<Object<'static>> {
    let HistoryRequest {
        project_id,
        project_schema,
    } = req;

    run_verb(env, host_driver, move |session| async move {
        let cfg = ExecutorConfig::new(project_id, project_schema);
        zero_migrate::ops::status::history(&session, &cfg)
            .await
            .map(|h| history_reply(&h))
            .map_err(|e| e.to_string())
    })
}
