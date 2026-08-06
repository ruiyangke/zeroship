//! The N-API transport — compiled only with the `napi` feature.
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
//! in the napi call). This is the fire-and-resolve topology.
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

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::Env;
use napi_derive::napi;

use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::{ApplyOutcome, LockMode};
use zero_migrate::apply::journal::{HistoryEvent, HistoryKind};
use zero_migrate::approval::Approval;
use zero_migrate::conn::ExecutorConfig;
use zero_migrate::model::migration::{Migration, MigrationId};
use zero_migrate::ops::status::{AppliedPlanStatus, MigrationStatus, PlanStatusManifest};
use zero_migrate::{LiveSchema, MigrationEngine, MigrationIr, SqlDialect, SqliteBackend};

use crate::api;
use crate::marshal::{JsError, JsReply, JsRequest};
use crate::runtime::run_engine_blocking;
use crate::session::{NapiHostSession, VerbDispatch, VerbReply};
use crate::wire::{
    ApplyIrSqliteRequest, ApplyPendingContractDto, ApplyReply, ApplyRequest, BlockedPlanDto,
    CollectionDescriptorDto, FieldDescriptorDto, GenArtifactsReply, GenArtifactsSource,
    HistoryEventDto, HistoryReply, HistoryRequest, LoadVerifyReply, PendingContractStatusDto,
    PlanStatusDto, PlanStatusStepDto, PreviewSqlSource, ResolvePendingRequest, RuntimeOptionsDto,
    StatusIrRequest, StatusReply, StatusRequest, UnexpectedJournalEntryDto,
};

/// The dialect a host-driven `apply` targets over the `SqlSession` seam. Only the
/// two NETWORK dialects reach the host driver — `SQLite` is in-process rusqlite and
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

fn charter_layer_refs(charter_layers: &[String]) -> Vec<&str> {
    charter_layers.iter().map(String::as_str).collect()
}

fn effective_policy_from_wire_layers(
    charter_layers: &[String],
) -> std::result::Result<zero_migrate::EffectivePolicy, String> {
    let layers = charter_layer_refs(charter_layers);
    zero_migrate::effective_policy_from_charter_layers(&layers)
}

fn preview_dialect(s: &str) -> std::result::Result<SqlDialect, String> {
    match s {
        "postgres" => Ok(SqlDialect::Postgres),
        "sqlite" => Ok(SqlDialect::Sqlite),
        "mysql" => Ok(SqlDialect::Mysql),
        other => Err(format!(
            "unknown dialect {other:?} (expected postgres|sqlite|mysql)"
        )),
    }
}

// ---------------------------------------------------------------------------
// Sync, DB-free entrypoints — inline on the napi call thread.
// ---------------------------------------------------------------------------

/// The IR-format version this addon was built against (fail-closed floor).
#[napi(js_name = "irVersion")]
#[must_use]
pub const fn ir_version() -> u32 {
    api::current_ir_version()
}

/// Load + verify an IR document (the sync, DB-free deploy gate). Returns a
/// typed [`LoadVerifyReply`]; never throws for a malformed document.
#[napi(js_name = "loadVerify")]
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
/// Runs inline on the napi call thread (no DB, no host driver). Returns a typed
/// [`GenArtifactsReply`]; `ok=false` + `error` on a malformed/incoherent source or a
/// malformed policy charter (never a throw). Exactly one of `envelopes`/`descriptors`
/// must be populated.
#[napi(js_name = "genArtifacts")]
#[must_use]
pub fn gen_artifacts(source: GenArtifactsSource) -> GenArtifactsReply {
    let GenArtifactsSource {
        envelopes,
        descriptors,
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
            api::gen_artifacts_from_envelopes(&envelopes, schema, &charter_refs)
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
            api::gen_artifacts_from_descriptors(&descriptors, schema, &charter_refs)
        }
    }
}

/// Render IR envelopes to SQL offline, without a database or host driver.
///
/// The ordered policy charter is composed into the same effective policy used by
/// apply/artifact generation. Each envelope is then lowered independently against
/// an empty schema through the core preview renderer, preserving input order and
/// its `[runtime-resolved]` labels for operations that require live catalog state.
#[napi(js_name = "previewSql")]
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

    envelopes
        .iter()
        .enumerate()
        .map(|(index, envelope)| {
            zero_migrate::render_ir_envelope_sql(envelope, dialect, &opts).map_err(|e| {
                Error::from_reason(format!(
                    "previewSql: envelope[{index}] failed to render: {e}"
                ))
            })
        })
        .collect()
}

fn gen_artifacts_err(msg: impl Into<String>) -> GenArtifactsReply {
    GenArtifactsReply {
        ok: false,
        env_db_ts: None,
        runtime_json: None,
        error: Some(msg.into()),
    }
}

/// Convert a boundary [`CollectionDescriptorDto`] into the engine's declarative
/// `CollectionDescriptor`. The rich sub-object facets crossed as `JsonValue`, so
/// they `serde_json::from_value` into their closed engine types verbatim.
fn descriptor_dto_to_engine(
    dto: CollectionDescriptorDto,
) -> std::result::Result<zero_migrate::render::declarative::CollectionDescriptor, String> {
    use zero_migrate::render::declarative::{CollectionDescriptor, IndexDescriptor};

    let fields = dto
        .fields
        .into_iter()
        .map(field_dto_to_engine)
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let indexes = dto
        .indexes
        .unwrap_or_default()
        .into_iter()
        .map(|i| IndexDescriptor {
            name: i.name,
            columns: i.columns,
            unique: i.unique.unwrap_or(false),
        })
        .collect();

    let runtime_options = runtime_options_dto_to_engine(dto.runtime_options)?;

    Ok(CollectionDescriptor {
        name: dto.name,
        owner_app: dto.owner_app,
        fields,
        indexes,
        runtime_options,
    })
}

fn field_dto_to_engine(
    dto: FieldDescriptorDto,
) -> std::result::Result<zero_migrate::render::declarative::FieldDescriptor, String> {
    use zero_migrate::model::ir::{GeneratedCol, IdentityCol};
    use zero_migrate::render::declarative::FieldDescriptor;

    let generated: Option<GeneratedCol> = match dto.generated {
        Some(v) => Some(
            serde_json::from_value(v)
                .map_err(|e| format!("field {:?}: invalid `generated` facet: {e}", dto.name))?,
        ),
        None => None,
    };
    let identity: Option<IdentityCol> = match dto.identity {
        Some(v) => Some(
            serde_json::from_value(v)
                .map_err(|e| format!("field {:?}: invalid `identity` facet: {e}", dto.name))?,
        ),
        None => None,
    };

    Ok(FieldDescriptor {
        name: dto.name,
        ty: dto.ty,
        required: dto.required.unwrap_or(false),
        unique: dto.unique.unwrap_or(false),
        references: dto.references,
        reference_column: None,
        reference_name: None,
        on_delete: dto.on_delete,
        on_update: dto.on_update,
        deferrable: dto.deferrable,
        literal_value: None,
        default: dto.default,
        min: dto.min,
        max: dto.max,
        enum_values: dto.enum_values,
        id_prefix: dto.id_prefix,
        vector_dims: dto.vector_dims,
        // The width facets stay unset on this path. The descriptor source only ever
        // reaches `gen_artifacts_from_descriptors`, which renders TypeScript types
        // and runtime JSON; `gen_types` reads neither the bounded length nor the
        // MySQL unbounded-TEXT spelling, both of which exist for DDL alone.
        char_len: None,
        max_length: None,
        unbounded_text: false,
        vector_metric: dto.vector_metric,
        case_sensitive: dto.case_sensitive,
        encrypted: dto.encrypted,
        mask: dto.mask,
        fts: dto.fts.unwrap_or(false),
        fts_language: dto.fts_language,
        generated,
        identity,
    })
}

fn runtime_options_dto_to_engine(
    dto: Option<RuntimeOptionsDto>,
) -> std::result::Result<zero_migrate::TableRuntimeOptions, String> {
    use zero_migrate::{TableRuntimeOptions, TableStrictness};
    let Some(dto) = dto else {
        return Ok(TableRuntimeOptions::default());
    };
    let strictness = match dto.strictness.as_deref() {
        None | Some("strict") => TableStrictness::Strict,
        Some("lenient") => TableStrictness::Lenient,
        Some("off") => TableStrictness::Off,
        Some(other) => {
            return Err(format!(
                "invalid runtime option strictness {other:?} (expected strict|lenient|off)"
            ))
        }
    };
    Ok(TableRuntimeOptions {
        soft_delete: dto.soft_delete.unwrap_or(false),
        versioning: dto.versioning.unwrap_or(false),
        strictness,
    })
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
        move |outcome: std::result::Result<T, String>| match outcome {
            Ok(reply) => deferred.resolve(move |_| Ok(reply)),
            Err(msg) => deferred.reject(Error::from_reason(msg)),
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

    run_engine_blocking(
        engine,
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

/// Project an [`ApplyOutcome`] and the lock-coherent outstanding rename set into
/// the typed [`ApplyReply`].
fn apply_reply(
    outcome: ApplyOutcome,
    pending_contracts: &[zero_migrate::PendingContract],
) -> ApplyReply {
    ApplyReply {
        applied: outcome.applied,
        skipped: outcome.skipped,
        recovered: outcome.recovered,
        pending_contracts: pending_contracts
            .iter()
            .map(|contract| ApplyPendingContractDto {
                table: contract.table.clone(),
                from_column: contract.from_col.clone(),
                to_column: contract.to_col.clone(),
                pending_version: contract.pending_version.clone(),
            })
            .collect(),
    }
}

/// Project a [`MigrationStatus`] into the typed [`StatusReply`] (the load-bearing
/// fields: current version + applied/pending/rolled-back version ids).
fn status_reply(s: &MigrationStatus) -> StatusReply {
    StatusReply {
        current_version: s.current_version.as_ref().map(|v| v.as_str().to_string()),
        applied: s.applied.iter().map(|e| e.version.clone()).collect(),
        pending: s.pending.iter().map(|v| v.as_str().to_string()).collect(),
        aborted: Vec::new(),
        rolled_back: s.rolled_back.iter().map(|e| e.version.clone()).collect(),
        pending_contracts: s
            .pending_contracts
            .iter()
            .map(|contract| PendingContractStatusDto {
                table: contract.table.clone(),
                pending_version: contract.pending_version.clone(),
                orphaned: contract.orphaned,
            })
            .collect(),
        blocked: s
            .blocked
            .iter()
            .map(|blocked| BlockedPlanDto {
                blocked: blocked.blocked.as_str().to_string(),
                dependency: blocked.dependency.as_str().to_string(),
                pending_version: blocked.pending_version.clone(),
            })
            .collect(),
        unexpected_journal: Vec::new(),
        plans: None,
    }
}

/// Project a complete-plan reconciliation into the shared status reply shape.
/// Top-level ids are LOGICAL PLAN ids; `plans[].steps` carries the actual journal
/// identities and their individual states.
fn plan_status_reply(status: &AppliedPlanStatus) -> StatusReply {
    let plans = status
        .plans
        .iter()
        .map(|plan| PlanStatusDto {
            version: plan.version.as_str().to_string(),
            name: plan.name.clone(),
            state: plan.state.as_str().to_string(),
            steps: plan
                .steps
                .iter()
                .map(|step| PlanStatusStepDto {
                    version: step.version.as_str().to_string(),
                    name: step.name.clone(),
                    kind: step.kind.as_str().to_string(),
                    state: step.state.as_str().to_string(),
                    cursor_stability_mode: step.cursor_stability_mode.clone(),
                    cursor_stability_invariant: step.cursor_stability_invariant.clone(),
                    writes_quiesced: step.writes_quiesced.clone(),
                })
                .collect(),
            missing_dependencies: plan
                .missing_dependencies
                .iter()
                .map(|dependency| dependency.as_str().to_string())
                .collect(),
        })
        .collect();
    StatusReply {
        current_version: status
            .current_version
            .as_ref()
            .map(|version| version.as_str().to_string()),
        applied: status
            .applied
            .iter()
            .map(|version| version.as_str().to_string())
            .collect(),
        pending: status
            .pending
            .iter()
            .map(|version| version.as_str().to_string())
            .collect(),
        aborted: status
            .aborted
            .iter()
            .map(|version| version.as_str().to_string())
            .collect(),
        rolled_back: status.rolled_back.clone(),
        pending_contracts: status
            .pending_contracts
            .iter()
            .map(|contract| PendingContractStatusDto {
                table: contract.table.clone(),
                pending_version: contract.pending_version.clone(),
                orphaned: contract.orphaned,
            })
            .collect(),
        blocked: status
            .blocked
            .iter()
            .map(|blocked| BlockedPlanDto {
                blocked: blocked.blocked.as_str().to_string(),
                dependency: blocked.dependency.as_str().to_string(),
                pending_version: blocked.pending_version.clone(),
            })
            .collect(),
        unexpected_journal: status
            .unexpected_journal
            .iter()
            .map(|entry| UnexpectedJournalEntryDto {
                version: entry.version.clone(),
                state: entry.state.as_str().to_string(),
                journal_checksum: entry.journal_checksum.clone(),
                journal_kind: entry.journal_kind.map(|kind| kind.as_str().to_string()),
            })
            .collect(),
        plans: Some(plans),
    }
}

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

/// Snapshot, lower, and apply one authored envelope inside one project-lock
/// bracket. The catalog facts used by lowering must describe the same serialized
/// database state that the executor mutates; taking the snapshot before the lock
/// would leave a check-then-use window for a concurrent deploy.
#[allow(clippy::too_many_arguments)]
async fn apply_ir_with_locked_backend<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    prior_envelope_json: &[String],
    envelope_json: &str,
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    charter_layers: &[String],
    approval: Approval,
    applied_by: &str,
) -> std::result::Result<ApplyReply, String> {
    let charter_refs = charter_layer_refs(charter_layers);
    backend
        .ensure_journal(cfg)
        .await
        .map_err(|error| error.to_string())?;
    backend
        .acquire_project_lock(cfg)
        .await
        .map_err(|error| format!("failed to acquire project lock: {error}"))?;

    let result = async {
        let snapshot = backend
            .snapshot_schema(cfg)
            .await
            .map_err(|error| format!("live schema introspection failed: {error}"))?;
        let journal_entries = backend
            .applied(cfg)
            .await
            .map_err(|error| error.to_string())?;
        let resolved_contracts = match backend.pending_contracts() {
            Some(capability) => capability
                .resolved_pending_contracts(cfg)
                .await
                .map_err(|error| error.to_string())?,
            None => Vec::new(),
        };
        let live = LiveSchema::from_catalog_snapshot(snapshot.clone(), owner_app);
        let artifact = if prior_envelope_json.is_empty() {
            match crate::lower::lower_envelope_to_plan_with_live(
                envelope_json,
                owner_app,
                project_schema,
                dialect,
                registry_json,
                &charter_refs,
                &live,
            ) {
                Ok(artifact) => artifact,
                Err(_) => {
                    let mut artifacts = crate::lower::lower_ordered_envelopes_to_plans_for_apply(
                        &[envelope_json.to_string()],
                        owner_app,
                        project_schema,
                        dialect,
                        registry_json,
                        &charter_refs,
                        snapshot,
                        &journal_entries,
                        &resolved_contracts,
                    )?;
                    artifacts.pop().ok_or_else(|| {
                        "lowering returned no plan for the migration envelope".to_string()
                    })?
                }
            }
        } else {
            let mut ordered_envelopes = prior_envelope_json.to_vec();
            ordered_envelopes.push(envelope_json.to_string());
            let mut artifacts = crate::lower::lower_ordered_envelopes_to_plans_for_apply(
                &ordered_envelopes,
                owner_app,
                project_schema,
                dialect,
                registry_json,
                &charter_refs,
                snapshot,
                &journal_entries,
                &resolved_contracts,
            )?;
            let manifests = artifacts
                .iter()
                .map(|artifact| {
                    PlanStatusManifest::from_applied_plan(&artifact.plan, &artifact.depends_on)
                        .map_err(|error| error.to_string())
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let status = zero_migrate::ops::status::status_plans_via_backend_locked(
                backend, cfg, &manifests,
            )
            .await
            .map_err(|error| error.to_string())?;
            crate::lower::require_applied_prefix(&manifests, prior_envelope_json.len(), &status)?;
            artifacts.pop().ok_or_else(|| {
                "lowering returned no plan for the current migration envelope".to_string()
            })?
        };
        let outcome = MigrationEngine::new()
            .apply_applied_plan_with_touched_and_depends(
                &artifact.plan,
                &artifact.touched_tables,
                &artifact.depends_on,
                approval,
                backend,
                cfg,
                applied_by,
                LockMode::AlreadyHeld,
            )
            .await
            .map_err(|error| error.to_string())?;
        let pending_contracts = match backend.pending_contracts() {
            Some(capability) => capability
                .outstanding_pending_contracts(cfg)
                .await
                .map_err(|error| error.to_string())?,
            None => Vec::new(),
        };
        Ok::<ApplyReply, String>(apply_reply(outcome.applied, &pending_contracts))
    }
    .await;

    let release = backend.release_project_lock(cfg).await;
    match (result, release) {
        (Ok(reply), Ok(())) => Ok(reply),
        (Ok(_), Err(error)) => Err(format!("failed to release project lock: {error}")),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => Err(format!(
            "{error}; additionally failed to release project lock: {release_error}"
        )),
    }
}

/// Lower and reconcile authored plans while holding the same project lock across
/// the live-catalog and journal reads.
#[allow(clippy::too_many_arguments)]
async fn status_ir_with_locked_backend<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    envelope_json: &[String],
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    charter_layers: &[String],
    read_only: bool,
) -> std::result::Result<StatusReply, String> {
    let charter_refs = charter_layer_refs(charter_layers);
    if !read_only {
        backend
            .ensure_journal(cfg)
            .await
            .map_err(|error| error.to_string())?;
    }
    backend
        .acquire_project_lock(cfg)
        .await
        .map_err(|error| format!("failed to acquire project lock: {error}"))?;

    let result = async {
        let snapshot = backend
            .snapshot_schema(cfg)
            .await
            .map_err(|error| format!("live schema introspection failed: {error}"))?;
        let journal_exists = if read_only {
            backend
                .journal_exists(cfg)
                .await
                .map_err(|error| error.to_string())?
        } else {
            true
        };
        let journal_entries = if journal_exists {
            backend
                .applied(cfg)
                .await
                .map_err(|error| error.to_string())?
        } else {
            Vec::new()
        };
        let resolved_contracts = if journal_exists {
            match backend.pending_contracts() {
                Some(capability) => capability
                    .resolved_pending_contracts(cfg)
                    .await
                    .map_err(|error| error.to_string())?,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let artifacts = crate::lower::lower_ordered_envelopes_to_plans(
            envelope_json,
            owner_app,
            project_schema,
            dialect,
            registry_json,
            &charter_refs,
            snapshot,
            &journal_entries,
            &resolved_contracts,
        )?;
        let manifests = artifacts
            .iter()
            .map(|artifact| {
                PlanStatusManifest::from_applied_plan(&artifact.plan, &artifact.depends_on)
                    .map_err(|error| error.to_string())
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let status = if read_only {
            zero_migrate::ops::status::status_plans_via_backend_read_only_locked(
                backend, cfg, &manifests,
            )
            .await
        } else {
            zero_migrate::ops::status::status_plans_via_backend_locked(backend, cfg, &manifests)
                .await
        }
        .map_err(|error| error.to_string())?;
        Ok::<StatusReply, String>(plan_status_reply(&status))
    }
    .await;

    let release = backend.release_project_lock(cfg).await;
    match (result, release) {
        (Ok(reply), Ok(())) => Ok(reply),
        (Ok(_), Err(error)) => Err(format!("failed to release project lock: {error}")),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => Err(format!(
            "{error}; additionally failed to release project lock: {release_error}"
        )),
    }
}

/// Read migration-only status through the selected dialect backend while holding
/// one project lock across the journal buckets. The core legacy status carrier
/// retains detailed PostgreSQL rollback rows, while the neutral backend trait
/// exposes rollback version ids; the Node reply needs only those ids, so project
/// them directly without sending PostgreSQL-only SQL to MySQL.
async fn legacy_status_with_locked_backend<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> std::result::Result<StatusReply, String> {
    backend
        .ensure_journal(cfg)
        .await
        .map_err(|error| error.to_string())?;
    backend
        .acquire_project_lock(cfg)
        .await
        .map_err(|error| format!("failed to acquire project lock: {error}"))?;

    let result = async {
        let status = zero_migrate::ops::status::status_via_backend_locked(backend, cfg, migrations)
            .await
            .map_err(|error| error.to_string())?;
        let rolled_back = backend
            .net_rolled_back_versions(cfg)
            .await
            .map_err(|error| error.to_string())?;
        let mut reply = status_reply(&status);
        reply.rolled_back = rolled_back;
        Ok::<StatusReply, String>(reply)
    }
    .await;

    let release = backend.release_project_lock(cfg).await;
    match (result, release) {
        (Ok(reply), Ok(())) => Ok(reply),
        (Ok(_), Err(error)) => Err(format!("failed to release project lock: {error}")),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => Err(format!(
            "{error}; additionally failed to release project lock: {release_error}"
        )),
    }
}

/// Resolve one durable PostgreSQL online-rename obligation and return the
/// remaining obligations from the same project-lock bracket.
#[allow(clippy::too_many_arguments)]
async fn resolve_pending_with_locked_backend<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    pending_version: &str,
    resolution: zero_migrate::Resolution,
    owner_app: &str,
    approval: Approval,
    applied_by: &str,
) -> std::result::Result<ApplyReply, String> {
    // Keep the approval failure DB-free. The engine enforces this again as a
    // defense in depth, but this adapter owns the outer lock bracket.
    if approval != Approval::Approved {
        return Err("explicit approval is required to resolve a pending contract".to_string());
    }

    backend
        .acquire_project_lock(cfg)
        .await
        .map_err(|error| format!("failed to acquire project lock: {error}"))?;

    let result = async {
        let outcome = MigrationEngine::new()
            .resolve_pending_contract_with_lock(
                pending_version,
                resolution,
                owner_app,
                approval,
                backend,
                cfg,
                applied_by,
                LockMode::AlreadyHeld,
            )
            .await
            .map_err(|error| error.to_string())?;
        let pending = backend
            .pending_contracts()
            .ok_or_else(|| "this backend does not support pending contracts".to_string())?
            .outstanding_pending_contracts(cfg)
            .await
            .map_err(|error| error.to_string())?;
        Ok::<ApplyReply, String>(apply_reply(outcome.applied, &pending))
    }
    .await;

    let release = backend.release_project_lock(cfg).await;
    match (result, release) {
        (Ok(reply), Ok(())) => Ok(reply),
        (Ok(_), Err(error)) => Err(format!("failed to release project lock: {error}")),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => Err(format!(
            "{error}; additionally failed to release project lock: {release_error}"
        )),
    }
}

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
#[napi(ts_return_type = "Promise<ApplyReply>")]
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
    let envelope_json = serde_json::to_string(&req.envelope)
        .map_err(|e| Error::from_reason(format!("envelope is not serializable: {e}")))?;
    let prior_envelope_json = req
        .prior_envelopes
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|envelope| {
            serde_json::to_string(envelope)
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
#[napi(js_name = "applyIrSqlite", ts_return_type = "Promise<ApplyReply>")]
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
        .map(|(index, envelope)| {
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
            pending_contracts: Vec::new(),
        })
    })
}

/// Complete or abort one outstanding PostgreSQL online column rename.
#[napi(ts_return_type = "Promise<ApplyReply>")]
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

/// The `project_id` an `ExecutorConfig` carries. The IR host path uses the project
/// schema as the project id (a fresh single-app project's schema == its id in the
/// create-first posture). A distinct project id can be threaded through a future
/// facade arg.
fn owner_app_project(project_schema: &str) -> String {
    project_schema.to_string()
}

/// `statusIr`: lower the supplied pure-JS envelopes through the same guarded
/// Rust path as [`apply_ir`], retain every executable plan step, and reconcile
/// their stable journal identities through the selected dialect backend.
///
/// This is the status entrypoint for mixed and data-only plans. The legacy
/// [`status`] verb remains available for callers that already hold a flat set of
/// pre-lowered `Migration` values.
#[napi(ts_return_type = "Promise<StatusReply>")]
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
        .map(|envelope| {
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
#[napi(js_name = "statusIrSqlite", ts_return_type = "Promise<StatusReply>")]
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
        .map(|envelope| {
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
/// typed [`StatusReply`].
#[napi(ts_return_type = "Promise<StatusReply>")]
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
#[napi(ts_return_type = "Promise<HistoryReply>")]
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

#[cfg(test)]
mod status_projection_tests {
    use super::*;
    use zero_migrate::apply::journal::JournaledKind;
    use zero_migrate::model::migration::MigrationId;
    use zero_migrate::ops::status::{BlockedPlan, PendingContractStatus, UnexpectedJournalEntry};

    #[test]
    fn plan_status_reply_preserves_operator_details() {
        let blocked_version = MigrationId::derive("node_status", b"blocked");
        let dependency = MigrationId::derive("node_status", b"dependency");
        let aborted_version = MigrationId::derive("node_status", b"aborted");
        let status = AppliedPlanStatus {
            current_version: None,
            applied: Vec::new(),
            pending: vec![blocked_version.clone()],
            aborted: vec![aborted_version.clone()],
            rolled_back: vec!["mig_rolled_back".to_string()],
            plans: Vec::new(),
            unexpected_journal: vec![UnexpectedJournalEntry {
                version: "mig_unexpected".to_string(),
                state: zero_migrate::ops::status::PlanStatusStepState::Applied,
                journal_checksum: "checksum".to_string(),
                journal_kind: Some(JournaledKind::Apply),
            }],
            pending_contracts: vec![PendingContractStatus {
                table: "widgets".to_string(),
                pending_version: "mig_pending_contract".to_string(),
                orphaned: false,
            }],
            blocked: vec![BlockedPlan {
                blocked: blocked_version.clone(),
                dependency: dependency.clone(),
                pending_version: "mig_pending_contract".to_string(),
            }],
        };

        let reply = plan_status_reply(&status);

        assert_eq!(reply.aborted, vec![aborted_version.as_str()]);

        assert_eq!(reply.rolled_back, ["mig_rolled_back"]);
        assert_eq!(reply.pending_contracts[0].table, "widgets");
        assert_eq!(reply.blocked[0].blocked, blocked_version.as_str());
        assert_eq!(reply.blocked[0].dependency, dependency.as_str());
        assert_eq!(reply.unexpected_journal[0].state, "applied");
        assert_eq!(
            reply.unexpected_journal[0].journal_kind.as_deref(),
            Some("apply")
        );
    }
}
