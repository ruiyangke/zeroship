//! The N-API transport — compiled only with the `napi` feature.
//!
//! It holds the Node ABI and the argument decoding around it. What each verb then
//! does with its decoded arguments lives in [`crate::verbs`], which carries no napi
//! type and is therefore tested in the napi-free build.
//!
//! ## Sync, DB-free entrypoints (run inline, no bridge)
//! `irVersion`, `loadVerify` — pure functions ([`crate::api`]). `loadVerify` returns
//! a typed [`LoadVerifyReply`] on the napi call thread (no JSON string).
//!
//! ## Async, host-driven entrypoints (fire-and-resolve)
//! `applyIr`, `apply`, `status`, `history` — each goes through the ONE generic
//! private `run_verb` helper: it builds a [`TsfnDispatch`] from the JS host-driver
//! callback, opens
//! a `create_deferred` promise, spawns the engine on its OWN std::thread
//! ([`crate::runtime::run_engine_blocking`]) running a reactor-less
//! `futures::executor::block_on`, and resolves/rejects the promise cross-thread with
//! a TYPED reply (`ApplyReply`/`StatusReply`/`HistoryReply`) when `block_on`
//! completes — NO `Promise<string>`, NO per-verb copy-pasted plumbing.
//!
//! The JS thread is **never** `join()`ed on the worker — that would deadlock
//! libuv/Bun (the host-driver TSFN callback can't run while the JS thread is parked
//! in the napi call). This is the fire-and-resolve topology.
//!
//! ## Panics never cross the FFI boundary
//! Every export carries `catch_unwind`. Without it a panic unwinds out of the
//! generated `extern "C"` shim and aborts the whole Node process - measured as
//! `fatal runtime error: failed to initiate panic, error 5, aborting` and a core
//! dump, with no JS stack and nothing for a caller to catch. napi-rs applies its
//! own `catch_unwind` only on that opt-in, so the attribute is load-bearing on
//! every one of these, not decoration.
//!
//! It covers what the worker-thread catch cannot: generated argument conversion,
//! the handwritten decode prefix each async verb runs before spawning, and return
//! conversion - all of which execute on the napi call thread.
//! [`crate::runtime::run_engine_blocking`] still owns the engine's own panics,
//! which become promise rejections rather than throws.
//!
//! ## The tokio-free verb bridge
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
//!
//! [`LoadVerifyReply`]: crate::wire::LoadVerifyReply
//! [`TsfnDispatch`]: crate::bridge::TsfnDispatch
//! [`TsfnDispatch::dispatch`]: crate::session::VerbDispatch::dispatch

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::Env;
use napi_derive::napi;

use zero_migrate::apply::journal::{HistoryEvent, HistoryKind};
use zero_migrate::approval::Approval;
use zero_migrate::conn::ExecutorConfig;
use zero_migrate::model::migration::{Migration, MigrationId};
use zero_migrate::{MigrationEngine, MigrationIr, SqlDialect, SqliteBackend};

use crate::api;
use crate::descriptors::descriptor_dto_to_engine;
use crate::marshal::{JsError, JsReply, JsRequest};
use crate::runtime::run_engine_blocking;
use crate::session::{NapiHostSession, VerbDispatch, VerbReply};
use crate::verbs::{
    apply_ir_with_locked_backend, charter_layer_refs, effective_policy_from_wire_layers,
    legacy_status_with_locked_backend, owner_app_project, parse_rollback_target, preview_dialect,
    resolve_pending_with_locked_backend, rollback_with_locked_backend,
    status_ir_with_locked_backend, ApplyDialect,
};
use crate::wire::{
    AdvisoryDto, ApplyIrSqliteRequest, ApplyReply, ApplyRequest, BuildInfo, GenArtifactsReply,
    GenArtifactsSource, HistoryEventDto, HistoryReply, HistoryRequest, LoadVerifyReply,
    PreviewSqlSource, ResolvePendingRequest, RollbackRequest, StatusIrRequest, StatusRequest,
};

// ---------------------------------------------------------------------------
// Sync, DB-free entrypoints — inline on the napi call thread.
// ---------------------------------------------------------------------------

/// The IR-format version this addon was built against (fail-closed floor).
#[napi(js_name = "irVersion", catch_unwind)]
#[must_use]
pub const fn ir_version() -> u32 {
    api::current_ir_version()
}

/// The loaded addon's build identity: crate version, IR floor, and the workspace
/// source digest. A host that resolves the `.node` by path can log this to prove
/// WHICH artifact it loaded, which the filename alone cannot say. Reproducible
/// from the committed tree; it does not report the toolchain or build profile.
#[napi(js_name = "buildInfo", catch_unwind)]
#[must_use]
pub fn build_info() -> BuildInfo {
    api::build_info()
}

/// Load + verify an IR document (the sync, DB-free deploy gate). Returns a
/// typed [`LoadVerifyReply`]; a malformed document is a reply, not a throw. An
/// engine panic is a throw, for the reason spelled out on [`gen_artifacts`].
#[napi(js_name = "loadVerify", catch_unwind)]
#[must_use]
pub fn load_verify(
    envelope_json: String,
    deploying_app: String,
    dialect: String,
    registry: std::collections::HashMap<String, String>,
    project_schema: String,
) -> LoadVerifyReply {
    api::load_verify(
        &envelope_json,
        &deploying_app,
        &dialect,
        &registry,
        &project_schema,
    )
}

/// `genArtifacts` — the sync, DB-free schema-artifact emitter. Fold a schema SOURCE
/// (EITHER a set of IR envelopes — the generated source — OR a declared
/// `CollectionDescriptor` set — the manual source) into the two co-emitted
/// artifacts `{ envDbTs, runtimeJson }`. Both sources funnel through the SAME Rust
/// renderer, so their output is byte-identical for equivalent schemas — PROVIDED the
/// same ordered `charterLayers` stack drives both (the confined system-shape injection is
/// policy-driven, not a baked-in engine preset; the caller supplies the confined
/// charter).
///
/// `dialect` is REQUIRED and names the project's real target: the fold selects
/// `Op::Dialectal` legs, so the artifacts are per-target, not portable.
///
/// Runs inline on the napi call thread (no DB, no host driver). Returns a typed
/// [`GenArtifactsReply`]; `ok=false` + `error` on a malformed/incoherent source, an
/// unknown dialect spelling, or a malformed policy charter. Exactly one of
/// `envelopes`/`descriptors` must be populated.
///
/// No INPUT reaches the caller as a throw. An engine panic does, and the two are
/// deliberately different shapes: `ok=false` says the source was rejected, a throw
/// says the engine broke. Folding a panic into `ok=false` would let a caller's
/// `if (!reply.ok)` branch report an internal defect as bad schema.
#[napi(js_name = "genArtifacts", catch_unwind)]
#[must_use]
pub fn gen_artifacts(source: GenArtifactsSource) -> GenArtifactsReply {
    let GenArtifactsSource {
        envelopes,
        descriptors,
        dialect,
        project_schema,
        charter_layers,
    } = source;
    let schema = project_schema.as_deref();
    let charter_refs = charter_layer_refs(&charter_layers);
    match (envelopes, descriptors) {
        (Some(_), Some(_)) => gen_artifacts_err(
            "genArtifacts: exactly one of `envelopes` (generated source) or `descriptors` \
             (manual source) must be set — both were provided",
        ),
        (None, None) => gen_artifacts_err(
            "genArtifacts: a source is required — set `envelopes` (generated source) or \
             `descriptors` (manual source)",
        ),
        (Some(envelopes), None) => {
            api::gen_artifacts_from_envelopes(&envelopes, &dialect, schema, &charter_refs)
        }
        (None, Some(dtos)) => {
            let descriptors = match dtos
                .into_iter()
                .map(descriptor_dto_to_engine)
                .collect::<std::result::Result<Vec<_>, _>>()
            {
                Ok(d) => d,
                Err(e) => return gen_artifacts_err(e),
            };
            api::gen_artifacts_from_descriptors(&descriptors, &dialect, schema, &charter_refs)
        }
    }
}

/// Render IR envelopes to SQL offline, without a database or host driver.
///
/// The ordered policy charter is composed into the same effective policy used by
/// apply/artifact generation. Each envelope is then lowered independently against
/// an empty schema through the core preview renderer, preserving input order and
/// its `[runtime-resolved]` labels for operations that require live catalog state.
#[napi(js_name = "previewSql", catch_unwind)]
pub fn preview_sql(source: PreviewSqlSource) -> Result<Vec<String>> {
    let PreviewSqlSource {
        envelopes,
        dialect,
        default_schema,
        owner_app,
        charter_layers,
    } = source;

    let dialect = preview_dialect(&dialect).map_err(Error::from_reason)?;
    let effective_policy = effective_policy_from_wire_layers(&charter_layers).map_err(|e| {
        Error::from_reason(format!("previewSql: policy charter failed to load: {e}"))
    })?;
    let opts = zero_migrate::PreviewOpts {
        default_schema,
        owner_app,
        effective_policy,
    };

    // Each envelope renders against the schema the ones BEFORE it leave behind,
    // the same way apply lowers them. Rendering every envelope against an empty
    // schema is what let `lint` report ok on a backfill whose cursor column is
    // declared by a `createTable` two files earlier and whose type `apply` then
    // refuses (F653): the rule was never unreachable, the column simply was not
    // in view.
    //
    // A fold that fails is NOT fatal here. This is an offline preview, and an op
    // the folder cannot model must not cost the operator the whole listing - the
    // render continues against the schema accumulated so far, which is exactly
    // how the per-op `[runtime-resolved]` degradation already behaves.
    let mut history: Vec<zero_migrate::model::ir::Op> = Vec::new();
    let mut out = Vec::with_capacity(envelopes.len());
    for (index, envelope) in envelopes.iter().enumerate() {
        let live = zero_migrate::render::fold::fold_ops(
            &history,
            dialect,
            &opts.default_schema,
            &opts.effective_policy,
        )
        .map_or_else(
            |_| zero_migrate::LiveSchema::default(),
            |snapshot| zero_migrate::LiveSchema::from_catalog_snapshot(snapshot, &opts.owner_app),
        );
        out.push(
            zero_migrate::render_ir_envelope_sql_onto(envelope, dialect, &opts, &live).map_err(
                |e| {
                    Error::from_reason(format!(
                        "previewSql: envelope[{index}] failed to render: {e}"
                    ))
                },
            )?,
        );
        if let Ok(ir) = serde_json::from_str::<zero_migrate::MigrationIr>(envelope) {
            history.extend(ir.ops);
        }
    }
    Ok(out)
}

fn gen_artifacts_err(msg: impl Into<String>) -> GenArtifactsReply {
    GenArtifactsReply {
        ok: false,
        env_db_ts: None,
        runtime_json: None,
        error: Some(msg.into()),
        // Refused at the boundary before any fold ran, so there is no answer to give.
        has_dialectal_ops: None,
        collections: None,
        dialect: None,
    }
}

// ---------------------------------------------------------------------------
// The host-driver TSFN payload + dispatch.
// ---------------------------------------------------------------------------

/// What crosses to the JS thread per verb: the request + the oneshot `Sender` the
/// `done` callback fires. Both are `Send + 'static` (owned data + a
/// `oneshot::Sender<VerbReply>`), so no `!Send` engine state crosses.
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
    VerbCall,            // T — payload crossing to the JS thread
    (),                  // Return — the host callback returns void; it calls `done` instead
    (JsRequest, DoneFn), // CallJsBackArgs
    Status,
    false, // CalleeHandled = false: we surface driver errors via `done(err, …)`
>;

/// A [`VerbDispatch`] that fires the host-driver `ThreadsafeFunction` and parks on a
/// oneshot the JS `done` callback fires.
pub struct TsfnDispatch {
    tsfn: HostTsfn,
}

impl std::fmt::Debug for TsfnDispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TsfnDispatch").finish_non_exhaustive()
    }
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
        // a JS Promise — keeps the reactor-less block_on sufficient.
        rx.await.unwrap_or_else(|_| {
            Err(JsError {
                message: "host driver dropped the `done` callback without replying".to_string(),
                code: None,
            })
        })
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
                        // that unparks the engine worker's block_on. A
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
/// via a raw `napi_value` round-trip. Sound because the `napi_value` outlives the
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
    let detached = unsafe {
        <Function<'static, Args, Ret> as FromNapiValue>::from_napi_value(env.raw(), raw)?
    };
    Ok(detached)
}

// ---------------------------------------------------------------------------
// Async, host-driven entrypoints — the ONE generic driver.
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

/// The ONE generic host-verb driver — the single home of the
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
/// the worker thread (the JS thread is never joined).
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
        move |outcome| match outcome {
            Ok(Ok(reply)) => deferred.resolve(move |_| Ok(reply)),
            Ok(Err(msg)) => deferred.reject(Error::from_reason(msg)),
            // A panic settles the promise too. A `JsDeferred` has no `Drop`, so
            // dropping this one would leave the caller awaiting forever with its
            // connection open and the project lock held.
            Err(panicked) => deferred.reject(Error::from_reason(panicked.reason())),
        },
    );

    detach_promise(env, promise)
}

/// The in-process peer of [`run_verb`]. It preserves the same deferred + dedicated
/// worker-thread topology but needs no [`NapiHostSession`]: bundled SQLite is
/// opened and driven entirely inside the Rust worker.
fn run_in_process_verb<T, Fut, E>(env: Env, engine: E) -> Result<Object<'static>>
where
    T: ToNapiValue + Send + 'static,
    Fut: Future<Output = std::result::Result<T, String>>,
    E: FnOnce() -> Fut + Send + 'static,
{
    let (deferred, promise) = env.create_deferred::<T, _>()?;

    run_engine_blocking(engine, move |outcome| match outcome {
        Ok(Ok(reply)) => deferred.resolve(move |_| Ok(reply)),
        Ok(Err(msg)) => deferred.reject(Error::from_reason(msg)),
        Err(panicked) => deferred.reject(Error::from_reason(panicked.reason())),
    });

    detach_promise(env, promise)
}

// ---------------------------------------------------------------------------
// The history projection. It stays with the ABI because `HistoryEventDto` carries
// the journal sequence as a napi `BigInt`; every other reply projection is plain
// data and lives in `crate::verbs`.
// ---------------------------------------------------------------------------

/// The wire spelling of a [`HistoryKind`] — the single home of the mapping (was a
/// closure-local `match` in the `history` entrypoint).
const fn history_kind_str(kind: HistoryKind) -> &'static str {
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
// The typed verbs — each is a thin `run_verb` closure over the engine.
// ---------------------------------------------------------------------------

/// `applyIr` — the HOST-AUTHORING apply entry: take a pure-JS IR envelope
/// ENVELOPE (`{ ir_version, name, ops }`) as a typed [`ApplyRequest`], run the
/// fail-closed LOAD GATE + LOWER **in Rust** (stamping `owner_app` + folding the
/// authoritative `Checksum::of_ir` — the checksum is NEVER computed in JS), then
/// drive the complete ordered plan over the host driver. The envelope must NOT carry
/// `owner_app`; it is stamped from `req.owner_app` (provenance).
///
/// This is the entry the `zero-migrate-cli` facade's `apply` calls: the pure-JS
/// recorder produces the envelope, this addon owns the checksum.
/// Resolves to a typed [`ApplyReply`].
#[napi(ts_return_type = "Promise<ApplyReply>", catch_unwind)]
pub fn apply_ir(
    env: Env,
    #[napi(
        ts_arg_type = "(args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void]) => void"
    )]
    host_driver: HostDriverFn,
    req: ApplyRequest,
) -> Result<Object<'static>> {
    // Lower the envelope to an ordered plan in Rust (checksum folded here). The
    // `ops` AST crossed as a real JS value; re-serialize it for the lower gate.
    //
    // Restore exact integers FIRST: a JS number above `u32::MAX` arrives as an f64,
    // and re-serializing it here would write `4294967296.0`, which the IR
    // deserializer refuses as fractional. `validate` never saw this because it hands
    // the addon a `JSON.stringify` of the same envelope.
    let mut envelope = req.envelope;
    crate::wire::restore_exact_integers(&mut envelope);
    let envelope_json = serde_json::to_string(&envelope)
        .map_err(|e| Error::from_reason(format!("envelope is not serializable: {e}")))?;
    let prior_envelope_json = req
        .prior_envelopes
        .unwrap_or_default()
        .into_iter()
        .map(|mut envelope| {
            crate::wire::restore_exact_integers(&mut envelope);
            serde_json::to_string(&envelope)
                .map_err(|e| Error::from_reason(format!("prior envelope is not serializable: {e}")))
        })
        .collect::<Result<Vec<_>>>()?;
    let registry_json = serde_json::to_string(&req.registry)
        .map_err(|e| Error::from_reason(format!("registry is not serializable: {e}")))?;
    // Dialect selects the backend: Postgres and MySQL ride the
    // SAME `SqlSession` seam, but each dialect's lock / journal / placeholder SQL
    // lives in its own `MigrationBackend`. `apply` builds `PostgresBackend`;
    // `apply_with_lock_mysql` builds `MysqlBackend` (`GET_LOCK`, MySQL journal DDL,
    // `?` placeholders). SQLite is in-process rusqlite and never reaches the host
    // seam, so it is not a valid host-driver dialect here.
    let target = ApplyDialect::parse(&req.dialect).map_err(Error::from_reason)?;

    let ApplyRequest {
        owner_app,
        project_schema,
        dialect,
        migrator_role,
        approved,
        applied_by,
        charter_layers,
        ..
    } = req;
    let approval = if approved {
        Approval::Approved
    } else {
        Approval::None
    };
    let effective =
        effective_policy_from_wire_layers(&charter_layers).map_err(Error::from_reason)?;

    run_verb(env, host_driver, move |session| async move {
        let mut cfg = ExecutorConfig::new(
            owner_app_project(&project_schema),
            project_schema.clone(),
            effective,
        );
        if let Some(role) = migrator_role {
            cfg = cfg.with_migrator_role(role);
        }
        match target {
            ApplyDialect::Postgres => {
                let backend = zero_migrate::PostgresBackend::new_generic(&session);
                apply_ir_with_locked_backend(
                    &backend,
                    &cfg,
                    &prior_envelope_json,
                    &envelope_json,
                    &owner_app,
                    &project_schema,
                    &dialect,
                    &registry_json,
                    &charter_layers,
                    approval,
                    &applied_by,
                )
                .await
            }
            ApplyDialect::Mysql => {
                let backend = zero_migrate::apply::backend::MysqlBackend::new_generic(&session);
                apply_ir_with_locked_backend(
                    &backend,
                    &cfg,
                    &prior_envelope_json,
                    &envelope_json,
                    &owner_app,
                    &project_schema,
                    &dialect,
                    &registry_json,
                    &charter_layers,
                    approval,
                    &applied_by,
                )
                .await
            }
        }
    })
}

/// `applyIrSqlite` — deploy an ordered migration-IR sequence through the bundled
/// in-process SQLite backend. There is no host-driver callback: the hardened app
/// and journal connections are opened on the engine worker thread, and the same
/// high-level library deploy loop used by Rust callers owns lowering, idempotent
/// journal skips, apply, and live-schema threading.
#[napi(
    js_name = "applyIrSqlite",
    ts_return_type = "Promise<ApplyReply>",
    catch_unwind
)]
pub fn apply_ir_sqlite(
    env: Env,
    app_path: String,
    journal_path: String,
    req: ApplyIrSqliteRequest,
) -> Result<Object<'static>> {
    let ApplyIrSqliteRequest {
        owner_app,
        project_schema,
        registry,
        charter_layers,
        approved,
        envelopes,
    } = req;

    let envelopes = envelopes
        .into_iter()
        .enumerate()
        .map(|(index, mut envelope)| {
            // Deserialized straight from the napi-converted value rather than via a
            // JSON string, so this is the site where a widened integer reaches the
            // IR first. Without the restore, a literal above `u32::MAX` arrives as
            // an f64 and is refused as fractional.
            crate::wire::restore_exact_integers(&mut envelope);
            serde_json::from_value::<MigrationIr>(envelope).map_err(|error| {
                Error::from_reason(format!(
                    "envelope at index {index} is not a MigrationIr document: {error}"
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let registry: BTreeMap<String, String> = registry.into_iter().collect();
    let effective =
        effective_policy_from_wire_layers(&charter_layers).map_err(Error::from_reason)?;
    let approval = if approved {
        Approval::Approved
    } else {
        Approval::None
    };

    run_in_process_verb(env, move || async move {
        let backend = SqliteBackend::open(Path::new(&app_path), Path::new(&journal_path))
            .map_err(|error| format!("failed to open SQLite migration backend: {error}"))?;
        let exec_cfg = ExecutorConfig::new(
            project_schema.clone(),
            project_schema.clone(),
            effective.clone(),
        );
        let outcome = MigrationEngine::new()
            .deploy_envelopes(
                &envelopes,
                &backend,
                &effective,
                SqlDialect::Sqlite,
                &project_schema,
                &owner_app,
                &registry,
                approval,
                &exec_cfg,
            )
            .await
            .map_err(|error| error.to_string())?;

        Ok(ApplyReply {
            applied: outcome.applied,
            skipped: outcome.skipped,
            recovered: outcome.recovered,
            // Empty because SQLite HAS no cross-deploy contracts, not because this
            // path drops them. `SqliteBackend::pending_contracts` returns `None`
            // (apply/backend/sqlite/mod.rs:1155): a rebuild rename is one atomic
            // offline step, so no obligation is ever opened. The networked verb
            // reaches the same value by asking - `None => Vec::new()` at
            // verbs.rs:296 - so the two replies agree today.
            //
            // They agree by coincidence of the answer, not by sharing the question.
            // Giving SQLite a contract partition would make verbs.rs report them and
            // leave this constant silently empty, so that change has to reach here.
            pending_contracts: Vec::new(),
        })
    })
}

/// Decode the shared parts of a rollback request into what the verb takes.
///
/// Both entrypoints need the same four conversions and the same refusals, and
/// duplicating them is how one path ends up accepting what the other rejects.
#[cfg(feature = "napi")]
struct DecodedRollback {
    envelope_json: Vec<String>,
    registry_json: String,
    target: zero_migrate::RollbackTarget,
    options: zero_migrate::RollbackOptions,
    approval: Approval,
}

#[cfg(feature = "napi")]
fn decode_rollback(req: &RollbackRequest) -> Result<DecodedRollback> {
    let envelope_json = req
        .envelopes
        .iter()
        .enumerate()
        .map(|(index, envelope)| {
            // Cloned because this decoder borrows the request and reads more of it
            // below; the normalization has to happen before re-serializing either
            // way, or a large integer literal in a rolled-back migration hits the
            // same fractional-number refusal `apply` used to.
            let mut envelope = envelope.clone();
            crate::wire::restore_exact_integers(&mut envelope);
            serde_json::to_string(&envelope).map_err(|error| {
                Error::from_reason(format!(
                    "envelope at index {index} is not serializable: {error}"
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let registry_json = serde_json::to_string(&req.registry)
        .map_err(|error| Error::from_reason(format!("registry is not serializable: {error}")))?;
    let target = parse_rollback_target(
        &req.target.kind,
        req.target.version.as_deref(),
        req.target.steps,
    )
    .map_err(Error::from_reason)?;
    // Forcing past a migration that declares no `down` discards data, so the force
    // flag alone is not enough for the engine and an operator who sets only that
    // one is told here rather than left wondering why the rollback still refused.
    if req.force && !req.backup_acknowledged {
        return Err(Error::from_reason(
            "force needs backupAcknowledged as well: skipping a migration that declares no \
             down discards the data it removed, so it takes an explicit acknowledgement that \
             a backup exists",
        ));
    }
    Ok(DecodedRollback {
        envelope_json,
        registry_json,
        target,
        options: zero_migrate::RollbackOptions {
            force: req.force,
            backup_acknowledged: req.backup_acknowledged,
        },
        approval: if req.approved {
            Approval::Approved
        } else {
            Approval::None
        },
    })
}

/// `rollback` - unwind applied migrations over the host driver.
///
/// The authored envelopes are lowered through the same guarded Rust path `applyIr`
/// uses, so the reverse SQL comes from the migration files rather than from
/// anything the journal stored. Resolves to a typed [`RollbackReply`].
///
/// [`RollbackReply`]: crate::wire::RollbackReply
#[napi(ts_return_type = "Promise<RollbackReply>", catch_unwind)]
pub fn rollback(
    env: Env,
    #[napi(
        ts_arg_type = "(args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void]) => void"
    )]
    host_driver: HostDriverFn,
    req: RollbackRequest,
) -> Result<Object<'static>> {
    let decoded = decode_rollback(&req)?;
    let target_backend = ApplyDialect::parse(&req.dialect).map_err(Error::from_reason)?;
    let RollbackRequest {
        owner_app,
        project_schema,
        migrator_role,
        dialect,
        charter_layers,
        applied_by,
        ..
    } = req;
    let effective =
        effective_policy_from_wire_layers(&charter_layers).map_err(Error::from_reason)?;

    run_verb(env, host_driver, move |session| async move {
        let mut cfg = ExecutorConfig::new(
            owner_app_project(&project_schema),
            project_schema.clone(),
            effective,
        );
        if let Some(role) = migrator_role {
            cfg = cfg.with_migrator_role(role);
        }
        match target_backend {
            ApplyDialect::Postgres => {
                let backend = zero_migrate::PostgresBackend::new_generic(&session);
                rollback_with_locked_backend(
                    &backend,
                    &cfg,
                    &decoded.envelope_json,
                    &owner_app,
                    &project_schema,
                    &dialect,
                    &decoded.registry_json,
                    &charter_layers,
                    decoded.target,
                    decoded.options,
                    decoded.approval,
                    &applied_by,
                )
                .await
            }
            ApplyDialect::Mysql => {
                let backend = zero_migrate::apply::backend::MysqlBackend::new_generic(&session);
                rollback_with_locked_backend(
                    &backend,
                    &cfg,
                    &decoded.envelope_json,
                    &owner_app,
                    &project_schema,
                    &dialect,
                    &decoded.registry_json,
                    &charter_layers,
                    decoded.target,
                    decoded.options,
                    decoded.approval,
                    &applied_by,
                )
                .await
            }
        }
    })
}

/// `rollbackSqlite` - unwind applied migrations through the bundled in-process
/// SQLite backend. There is no host-driver callback.
#[napi(
    js_name = "rollbackSqlite",
    ts_return_type = "Promise<RollbackReply>",
    catch_unwind
)]
pub fn rollback_sqlite(
    env: Env,
    app_path: String,
    journal_path: String,
    req: RollbackRequest,
) -> Result<Object<'static>> {
    let decoded = decode_rollback(&req)?;
    let RollbackRequest {
        owner_app,
        project_schema,
        migrator_role,
        dialect,
        charter_layers,
        applied_by,
        ..
    } = req;
    if dialect != "sqlite" {
        return Err(Error::from_reason(format!(
            "rollbackSqlite requires dialect \"sqlite\" (got {dialect:?})"
        )));
    }
    // SQLite has no roles to assume. Accepting one here would let a caller believe
    // the reverse DDL runs least-privilege when it runs as the only identity there is.
    if migrator_role.is_some() {
        return Err(Error::from_reason(
            "rollbackSqlite takes no migratorRole: SQLite has no roles, so the reverse DDL \
             cannot be run under a narrower identity than the connection's own",
        ));
    }
    let effective =
        effective_policy_from_wire_layers(&charter_layers).map_err(Error::from_reason)?;

    run_in_process_verb(env, move || async move {
        let backend = SqliteBackend::open(Path::new(&app_path), Path::new(&journal_path))
            .map_err(|error| format!("failed to open SQLite migration backend: {error}"))?;
        let cfg = ExecutorConfig::new(
            owner_app_project(&project_schema),
            project_schema.clone(),
            effective,
        );
        rollback_with_locked_backend(
            &backend,
            &cfg,
            &decoded.envelope_json,
            &owner_app,
            &project_schema,
            &dialect,
            &decoded.registry_json,
            &charter_layers,
            decoded.target,
            decoded.options,
            decoded.approval,
            &applied_by,
        )
        .await
    })
}

/// Complete or abort one outstanding PostgreSQL online column rename.
#[napi(ts_return_type = "Promise<ApplyReply>", catch_unwind)]
pub fn resolve_pending(
    env: Env,
    #[napi(
        ts_arg_type = "(args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void]) => void"
    )]
    host_driver: HostDriverFn,
    req: ResolvePendingRequest,
) -> Result<Object<'static>> {
    let ResolvePendingRequest {
        owner_app,
        project_schema,
        migrator_role,
        pending_version,
        action,
        approved,
        applied_by,
        charter_layers,
    } = req;

    MigrationId::parse(&pending_version)
        .map_err(|error| Error::from_reason(format!("invalid pending version: {error}")))?;
    let resolution = match action.as_str() {
        "apply" => zero_migrate::Resolution::Applied,
        "abort" => zero_migrate::Resolution::Aborted,
        other => {
            return Err(Error::from_reason(format!(
                "unknown pending-contract action {other:?} (expected apply|abort)"
            )))
        }
    };
    let approval = if approved {
        Approval::Approved
    } else {
        Approval::None
    };
    let effective =
        effective_policy_from_wire_layers(&charter_layers).map_err(Error::from_reason)?;

    run_verb(env, host_driver, move |session| async move {
        let mut cfg = ExecutorConfig::new(
            owner_app_project(&project_schema),
            project_schema.clone(),
            effective,
        );
        if let Some(role) = migrator_role {
            cfg = cfg.with_migrator_role(role);
        }
        let backend = zero_migrate::PostgresBackend::new_generic(&session);
        resolve_pending_with_locked_backend(
            &backend,
            &cfg,
            &pending_version,
            resolution,
            &owner_app,
            approval,
            &applied_by,
        )
        .await
    })
}

/// `statusIr`: lower the supplied pure-JS envelopes through the same guarded
/// Rust path as [`apply_ir`], retain every executable plan step, and reconcile
/// their stable journal identities through the selected dialect backend.
///
/// This is the status entrypoint for mixed and data-only plans. The legacy
/// [`status`] verb remains available for callers that already hold a flat set of
/// pre-lowered `Migration` values.
#[napi(ts_return_type = "Promise<StatusReply>", catch_unwind)]
pub fn status_ir(
    env: Env,
    #[napi(
        ts_arg_type = "(args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void]) => void"
    )]
    host_driver: HostDriverFn,
    req: StatusIrRequest,
) -> Result<Object<'static>> {
    let StatusIrRequest {
        owner_app,
        project_schema,
        dialect,
        registry,
        envelopes,
        charter_layers,
        read_only,
    } = req;
    let read_only = read_only.unwrap_or(false);
    let registry_json = serde_json::to_string(&registry)
        .map_err(|e| Error::from_reason(format!("registry is not serializable: {e}")))?;

    let envelope_json = envelopes
        .into_iter()
        .map(|mut envelope| {
            crate::wire::restore_exact_integers(&mut envelope);
            serde_json::to_string(&envelope)
                .map_err(|e| Error::from_reason(format!("envelope is not serializable: {e}")))
        })
        .collect::<Result<Vec<_>>>()?;
    let target = ApplyDialect::parse(&dialect).map_err(Error::from_reason)?;
    let effective =
        effective_policy_from_wire_layers(&charter_layers).map_err(Error::from_reason)?;

    run_verb(env, host_driver, move |session| async move {
        let cfg = ExecutorConfig::new(
            owner_app_project(&project_schema),
            project_schema.clone(),
            effective,
        );
        match target {
            ApplyDialect::Postgres => {
                let backend = zero_migrate::PostgresBackend::new_generic(&session);
                status_ir_with_locked_backend(
                    &backend,
                    &cfg,
                    &envelope_json,
                    &owner_app,
                    &project_schema,
                    &dialect,
                    &registry_json,
                    &charter_layers,
                    read_only,
                )
                .await
            }
            ApplyDialect::Mysql => {
                let backend = zero_migrate::apply::backend::MysqlBackend::new_generic(&session);
                status_ir_with_locked_backend(
                    &backend,
                    &cfg,
                    &envelope_json,
                    &owner_app,
                    &project_schema,
                    &dialect,
                    &registry_json,
                    &charter_layers,
                    read_only,
                )
                .await
            }
        }
    })
}

/// `statusIrSqlite`: reconcile authored plans through the bundled in-process
/// SQLite backend. The journal is bootstrapped by default, matching [`status_ir`];
/// a request with `readOnly: true` uses the non-creating journal-existence path.
#[napi(
    js_name = "statusIrSqlite",
    ts_return_type = "Promise<StatusReply>",
    catch_unwind
)]
pub fn status_ir_sqlite(
    env: Env,
    app_path: String,
    journal_path: String,
    req: StatusIrRequest,
) -> Result<Object<'static>> {
    let StatusIrRequest {
        owner_app,
        project_schema,
        dialect,
        registry,
        envelopes,
        charter_layers,
        read_only,
    } = req;
    if dialect != "sqlite" {
        return Err(Error::from_reason(format!(
            "statusIrSqlite requires dialect \"sqlite\" (got {dialect:?})"
        )));
    }
    let read_only = read_only.unwrap_or(false);
    let registry_json = serde_json::to_string(&registry)
        .map_err(|error| Error::from_reason(format!("registry is not serializable: {error}")))?;
    let envelope_json = envelopes
        .into_iter()
        .map(|mut envelope| {
            crate::wire::restore_exact_integers(&mut envelope);
            serde_json::to_string(&envelope).map_err(|error| {
                Error::from_reason(format!("envelope is not serializable: {error}"))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let effective =
        effective_policy_from_wire_layers(&charter_layers).map_err(Error::from_reason)?;

    run_in_process_verb(env, move || async move {
        let backend = SqliteBackend::open(Path::new(&app_path), Path::new(&journal_path))
            .map_err(|error| format!("failed to open SQLite migration backend: {error}"))?;
        let cfg = ExecutorConfig::new(
            owner_app_project(&project_schema),
            project_schema.clone(),
            effective,
        );
        status_ir_with_locked_backend(
            &backend,
            &cfg,
            &envelope_json,
            &owner_app,
            &project_schema,
            &dialect,
            &registry_json,
            &charter_layers,
            read_only,
        )
        .await
    })
}

/// `status` — the generic `ops::status::status` over the host driver.
/// Migrations cross as a typed `Vec<JsonValue>` (each a `Migration`). Resolves to a
/// typed [`StatusReply`](crate::wire::StatusReply).
#[napi(ts_return_type = "Promise<StatusReply>", catch_unwind)]
pub fn status(
    env: Env,
    #[napi(
        ts_arg_type = "(args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void]) => void"
    )]
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
        dialect,
        charter_layers,
        ..
    } = req;
    let target = ApplyDialect::parse(&dialect).map_err(Error::from_reason)?;
    let effective =
        effective_policy_from_wire_layers(&charter_layers).map_err(Error::from_reason)?;

    run_verb(env, host_driver, move |session| async move {
        let cfg = ExecutorConfig::new(project_id, project_schema, effective);
        match target {
            ApplyDialect::Postgres => {
                let backend = zero_migrate::PostgresBackend::new_generic(&session);
                legacy_status_with_locked_backend(&backend, &cfg, &migrations).await
            }
            ApplyDialect::Mysql => {
                let backend = zero_migrate::apply::backend::MysqlBackend::new_generic(&session);
                legacy_status_with_locked_backend(&backend, &cfg, &migrations).await
            }
        }
    })
}

/// `history` — the generic `ops::status::history` over the host driver.
/// Resolves to a typed [`HistoryReply`].
#[napi(ts_return_type = "Promise<HistoryReply>", catch_unwind)]
pub fn history(
    env: Env,
    #[napi(
        ts_arg_type = "(args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void]) => void"
    )]
    host_driver: HostDriverFn,
    req: HistoryRequest,
) -> Result<Object<'static>> {
    let HistoryRequest {
        project_id,
        project_schema,
        charter_layers,
    } = req;
    let effective =
        effective_policy_from_wire_layers(&charter_layers).map_err(Error::from_reason)?;

    run_verb(env, host_driver, move |session| async move {
        let cfg = ExecutorConfig::new(project_id, project_schema, effective);
        zero_migrate::ops::status::history(&session, &cfg)
            .await
            .map(|h| history_reply(&h))
            .map_err(|e| e.to_string())
    })
}

/// The operational advisories the analyzer finds in an envelope set's lowered
/// DDL, attributed to the statement that raised each one.
///
/// F650. The engine already computed these and threw them away: `analyze`
/// produces an ACCESS EXCLUSIVE warning for `ALTER TABLE … ADD CONSTRAINT …
/// UNIQUE`, the declarative differ exposes them, and no CLI verb ever read one.
/// An operator adding a unique column to a populated table took a table-wide
/// lock with nothing anywhere telling them it was coming.
///
/// Advisories NEVER gate. They are information an operator reads before choosing
/// to deploy, which is why the statement travels with them: a warning that does
/// not say WHICH table it is about cannot be acted on at 2am.
#[napi(js_name = "advisoriesFor", catch_unwind)]
pub fn advisories_for(source: PreviewSqlSource) -> Result<Vec<AdvisoryDto>> {
    let PreviewSqlSource {
        envelopes,
        dialect,
        default_schema,
        owner_app,
        charter_layers,
    } = source;

    let dialect = preview_dialect(&dialect).map_err(Error::from_reason)?;
    let effective_policy = effective_policy_from_wire_layers(&charter_layers).map_err(|e| {
        Error::from_reason(format!("advisoriesFor: policy charter failed to load: {e}"))
    })?;
    let opts = zero_migrate::PreviewOpts {
        default_schema,
        owner_app,
        effective_policy,
    };

    let mut out = Vec::new();

    // F657. The analyzer parses PostgreSQL. MySQL renders identifiers with
    // backticks, which is not valid PostgreSQL, so every statement fails to parse
    // and `analyze` returns an empty vector - for SQL THIS ENGINE EMITS and is
    // about to run. The result was a clean advisory report on MySQL that meant
    // "could not read any of this", indistinguishable from "looked and found
    // nothing". Say which one it is; an operator reading a silent report is
    // entitled to know the analyzer never spoke.
    if !matches!(dialect, zero_migrate::SqlDialect::Postgres) {
        out.push(AdvisoryDto {
            migration: String::new(),
            rule: "analyzer_dialect_unsupported".to_string(),
            severity: "notice".to_string(),
            message: format!(
                "operational advisories are not available for {}: the analyzer reads \
                 PostgreSQL syntax, so no rule was evaluated against these statements. \
                 An empty advisory list here means UNCHECKED, not clean",
                dialect_wire_name(dialect)
            ),
            suggestion: None,
            statement: String::new(),
        });
        return Ok(out);
    }

    for envelope in &envelopes {
        // Statement-at-a-time so each advisory keeps the statement that raised
        // it. `analyze` over a whole multi-statement `up` would return a flat
        // list with no way back to the ALTER TABLE it describes.
        let Ok((migration, statements)) =
            zero_migrate::render_ir_envelope_sql_statements(envelope, dialect, &opts)
        else {
            // An envelope that will not render offline yields no advisories
            // rather than failing the verb: this is enrichment, never a gate.
            continue;
        };
        for statement in statements {
            for advisory in zero_migrate::analysis::analyze::analyze(&statement) {
                out.push(AdvisoryDto {
                    migration: migration.clone(),
                    rule: advisory.rule.to_string(),
                    severity: format!("{:?}", advisory.severity).to_lowercase(),
                    message: advisory.message,
                    suggestion: advisory.suggestion,
                    statement: statement.clone(),
                });
            }
        }
    }
    Ok(out)
}

/// The wire spelling of a dialect, for operator-facing text.
const fn dialect_wire_name(dialect: zero_migrate::SqlDialect) -> &'static str {
    match dialect {
        zero_migrate::SqlDialect::Postgres => "postgres",
        zero_migrate::SqlDialect::Mysql => "mysql",
        zero_migrate::SqlDialect::Sqlite => "sqlite",
    }
}
