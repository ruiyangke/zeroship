//! `db.registerModel(collection, schema, indexes)` — the four-phase DDL
//! pipeline.
//!
//! Proposal A2 (docs/proposals/zeroship-db.md) defines the contract:
//!
//! 1. **Bootstrap** (`bootstrap`) — create the per-app schema, the
//!    `__zeroship_migrations` audit table (idempotent), acquire the
//!    session-scoped advisory lock, compute the deploy's `schema_version`,
//!    expand declared + named indexes. Returns a `RegisterContext` the
//!    later stages thread through.
//! 2. **Plan** (`plan`) — introspect the live schema, diff against the
//!    declared schema, classify each change as additive / compatible /
//!    destructive. Returns a `Plan`.
//! 3. **Validate** (`validate`) — apply safety rules. Destructive ops
//!    under `strictness=strict` produce a `validation_refused` envelope
//!    the SDK consumes verbatim. Lenient deploys log + skip; off
//!    proceeds.
//! 4. **Apply** (`apply`) — two-pass DDL execution under the advisory
//!    lock (transactional ops first; `CREATE INDEX CONCURRENTLY` after
//!    releasing the lock). Every op writes an audit row through the
//!    [`crate::backend::Backend`] facade.
//!
//! Each submodule is `pub(crate)` so integration tests can call into the
//! stages independently. The V8-facing surface is
//! [`register_model_dispatch`] — unchanged from before the split.
//!
//! ## Error rail
//!
//! Every fallible function here — including the three pipeline sequencers
//! (`exec_register_model`, `run_pipeline`, `exec_register_model_with_pool`)
//! and every stage submodule (`bootstrap`, `plan`, `apply`) — returns
//! `Result<_, crate::error::DbError>`. The dispatch site renders the
//! typed error through [`crate::error::DbError::to_op_error`] so the JS
//! exception carries `.code` per variant (`lock_not_available`,
//! `transient`, `lazy_init_failed`, etc.).
//!
//! The `validation_refused` envelope wire shape is preserved as the lone
//! exception: `validate::validate` still returns `Err(envelope_json: String)`
//! because the JSON body is the documented SDK contract. We wrap it at
//! the boundary as [`crate::error::DbError::SchemaRefused`], whose
//! `to_op_error()` arm materialises a plain `Error` with `message =
//! envelope_json` — the SDK still does `JSON.parse(err.message)` exactly
//! as before.

use serde_json::Value;
use zeroship_runtime::state::{OpResult, ResolveValue};

use crate::backend::RegisterBackend;
use crate::context;
use crate::error::DbError;
use crate::v8_bridge::{runtime_state, setup_js_promise};

pub(crate) mod apply;
pub(crate) mod bootstrap;
pub(crate) mod plan;
pub(crate) mod validate;

/// `zeroship.db.registerModel(collection, schemaJson)` → `Promise<void>`
///
/// Creates the table and any missing columns. Idempotent — safe to call
/// on every cold start. Skips DDL if the model was already registered
/// for this app on this thread.
pub fn register_model_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    schema: Value,
    indexes: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);

    // Fast path: already registered on this thread — skip DDL.
    if crate::is_model_registered(app_id, collection) {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let undefined = v8::undefined(scope);
        resolver.resolve(scope, undefined.into());
        return promise;
    }

    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    let app_id_owned = app_id.to_string();
    let collection_owned = collection.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_register_model(&app_id_owned, &collection_owned, &schema, &indexes).await {
            Ok(()) => {
                crate::mark_model_registered(&app_id_owned, &collection_owned);
                // **P5 PR 2** — cache the schema so the CRUD encryption
                // pass can find `t.encrypted(...)` columns at dispatch
                // time. Cloned because the closure captures `schema` by
                // move; the cache is per-isolate and lives for the
                // isolate's lifetime (no eviction).
                //
                // **P7 PR 4** — stamp the `_systemFields: true` marker
                // so the CRUD update pass can distinguish a freshly-
                // registered (PR 2-emitted) table from a pre-migration
                // legacy table. The marker is read by
                // [`crate::crud::system_fields_pass::check_system_fields_marker`];
                // its absence in the cached schema surfaces a typed
                // `system_fields_missing` error so the SDK can guide the
                // creator to re-register or wait for PR 6's ALTER pass.
                // The marker is namespaced under a `_` prefix matching
                // the `_meta` / `_indexes` skip list in
                // [`crate::query::is_schema_metadata_key`] so the
                // declaration-time field validators ignore it.
                let mut cached = schema.clone();
                if let Some(obj) = cached.as_object_mut() {
                    obj.insert(
                        "_systemFields".to_string(),
                        serde_json::Value::Bool(true),
                    );
                }
                context::with_mut(|c| {
                    c.cache_schema(&app_id_owned, &collection_owned, cached);
                });
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::String("null".to_string()),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            },
        }
    }));

    promise
}

/// Lazily initialise the pool, resolve the deploy id, then drive the
/// four-phase pipeline through the backend.
async fn exec_register_model(
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
) -> Result<(), DbError> {
    // Lazy pool init
    let has_pool = context::with(|c| c.pool_initialised());
    if !has_pool {
        crate::init_pool_async()
            .await
            .map_err(|e| DbError::config("lazy_init_failed", format!("db: lazy init failed: {e}")))?;
    }

    let backend = context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("backend_not_initialized", "db: backend not initialized"))?;
    // P0 PR 5: `BackendHandle` is the enum (no `dyn Backend`). The
    // PG-only register-model pipeline pulls a `&PostgresBackend` out
    // of the enum via `as_postgres()` for the duration of the
    // `run_pipeline` await — async-friendly shape (closure-based
    // `with_postgres` can't span `.await` ergonomically).
    //
    // Post-P0 mop-up (MAJOR-R14-1): map the `None` arm to a typed
    // `backend_unsupported` `DbError::Configuration` so a future SQLite
    // arm surfaces a coded SDK-visible error rather than aborting the
    // spawned compio task via `.expect()` panic.
    let pg = backend
        .as_postgres()
        .ok_or_else(|| DbError::backend_unsupported("register_model"))?;

    let deploy_id =
        std::env::var("ZEROSHIP_DEPLOY_ID").unwrap_or_else(|_| "cold_start".to_string());

    run_pipeline(pg, app_id, collection, schema, indexes, &deploy_id).await
}

/// Backend-driven variant of `exec_register_model`. Public so
/// integration tests can drive the four-phase orchestrator without
/// going through V8.
///
/// `deploy_id` controls audit-log grouping (proposal A3 line 233
/// reserves `'cold_start'` for pre-deploy DDL).
///
/// The four stages are:
///
/// ```text
/// bootstrap → plan → validate → apply
/// ```
///
/// Each stage is implemented as a free function in its own submodule;
/// this entry just sequences them. The advisory-lock client returned
/// from `bootstrap` lives until `apply` releases the lock between
/// passes.
///
/// **P0 PR 3**: generic over [`RegisterBackend`] (was concrete
/// `&PostgresBackend`). The PG pool's `get()` lives behind
/// [`crate::backend::PgLockManager::acquire_pooled_client_for_lock`]
/// now, so the `PooledClient<'p>` lifetime still threads through to
/// `apply` but no concrete-type leak remains in this signature. Open
/// Q5 resolution per `docs/proposals/p0-implementation-plan.md`
/// §"PR 3" + §3 Q5 and `docs/proposals/db-system-design.md` §7.
pub async fn run_pipeline<B: RegisterBackend>(
    backend: &B,
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
    deploy_id: &str,
) -> Result<(), DbError> {
    // 1. Bootstrap — schema, audit table, advisory lock, schema_version,
    //    expanded index specs. The returned guard carries the pool
    //    borrow lifetime; we thread it through to apply where the
    //    advisory unlock happens between passes.
    let (ctx, lock_guard) =
        bootstrap::bootstrap(backend, app_id, collection, schema, indexes, deploy_id).await?;

    // 2 + 3. Plan / validate. These two stages happen BEFORE the
    // apply-side advisory-unlock. If either returns Err we must still
    // release the advisory lock via the guard — otherwise the held
    // PooledClient returns to the pool with the session-scoped lock
    // alive, and every later caller hangs on
    // `pg_advisory_lock(zs_reg:<app>, register_model)`.
    //
    // The pre-guard code propagated plan/validate errors via `?`,
    // dropping `lock_client` back into the pool without an explicit
    // unlock. The advisory lock leak cascades into the p8a2 ordering
    // hang: tests that `expect_err` on a destructive deploy (strict
    // refusal at validate) leave the orchestrator lock stuck, the next
    // register_model in another test waits forever, and the
    // `pg_create_logical_replication_slot()` in p8a2_auto_spawn
    // ultimately blocks behind that chain. The guard centralises the
    // unlock so any future stage added here inherits the invariant.
    //
    // Validate's `Err` branch is always the `validation_refused` JSON
    // envelope (the only fallible call inside it is a best-effort
    // audit write under `tracing::warn`). Wrap that envelope in
    // `DbError::SchemaRefused` so the dispatch boundary's
    // `to_op_error()` materialises a plain `Error` with `message =
    // envelope` — preserves the documented `JSON.parse(err.message)`
    // SDK contract while keeping the rest of the pipeline on the
    // typed rail.
    let plan_res = plan::compute_plan(backend, &ctx, collection, schema).await;
    let approved_res = match plan_res {
        Ok(plan) => validate::validate(backend, &ctx, plan)
            .await
            .map_err(|envelope_json| DbError::SchemaRefused {
                code: "validation_refused",
                envelope_json,
            }),
        Err(e) => Err(e),
    };
    let approved = match approved_res {
        Ok(a) => a,
        Err(e) => {
            // Plan/validate failed — release the advisory lock before
            // returning the PooledClient to the pool. Best-effort:
            // the unlock SQL may itself error if the connection was
            // already torn down, but the more important guarantee is
            // that we don't leak a held lock back into the pool.
            let _ = lock_guard.release().await;
            return Err(e);
        }
    };

    // 4. Apply — execute the validated ops under the lock (pass 1) then
    //    release and run CIC unlocked (pass 2). Each op writes an audit
    //    row. The guard's release lives inside apply().
    apply::apply(backend, ctx, lock_guard, approved).await
}

/// Pool-driven entry retained for integration tests that hand in a
/// `Rc<Pool>` directly (predates the Backend trait). Builds an ad-hoc
/// [`PostgresBackend`] around the pool and delegates to
/// [`run_pipeline`].
///
/// Production code reaches the orchestrator through
/// [`register_model_dispatch`] / [`exec_register_model`], which read
/// the backend from the per-isolate context. Test code that doesn't
/// drive the V8 lifecycle uses this helper to skip the lookup.
#[cfg(any(test, feature = "test-helpers"))]
pub async fn exec_register_model_with_pool(
    pool: std::rc::Rc<compio_postgres::Pool>,
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
    deploy_id: &str,
) -> Result<(), DbError> {
    let url = context::with(|c| c.db_url()).unwrap_or_default();
    let backend = crate::backend::PostgresBackend::new(pool, url);
    run_pipeline(&backend, app_id, collection, schema, indexes, deploy_id).await
}
