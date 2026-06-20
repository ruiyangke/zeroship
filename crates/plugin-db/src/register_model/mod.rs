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

use crate::backend::{AuditWriter, DialectBuilder, RegisterBackend};
use crate::context;
use crate::error::DbError;
use crate::v8_bridge::{runtime_state, setup_js_promise};

pub(crate) mod apply;
pub(crate) mod bootstrap;
pub(crate) mod plan;
pub(crate) mod sqlite_engine;
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
                context::with_mut(|c| {
                    c.cache_schema(&app_id_owned, &collection_owned, schema.clone());
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

    // **P5 — the cutover (dialect-conditional; do NOT brick SQLite dev).** The
    // schema-authority split (`docs/proposals/2026-06-18-schema-authority-drizzle-
    // model-design.md` §6/§9/§12 P5) makes `zeroship-migrate` the SOLE PG schema
    // applier: the engine creates/migrates the per-app PG schema (and provisions
    // the `migrator_<app_id>` role) at DEPLOY, BEFORE go-live (P6,
    // `control`'s `deploy_migrate::apply_bundle_migrations`). So on the PG dialect
    // `registerModel` STOPS being a schema authority — it issues NO runtime DDL
    // (no `bootstrap` create-schema / `plan` / `validate` / `apply`). This is what
    // eliminates the two-applier overlap (plugin-db + engine both running DDL
    // under disjoint advisory-lock namespaces — the prior design's CRITICAL #1).
    //
    // What the PG no-DDL path STILL guarantees — the "schema-ready + metadata-
    // available" contract the P4 introspection cache depends on (design §6):
    //   * readiness — the dispatch caller (`register_model_dispatch`) marks
    //     `is_model_registered` on this `Ok(())`, which is the gate
    //     `crud::introspect_schema::runtime_schema_for` checks before sourcing
    //     per-collection metadata from LIVE introspection + the engine's sentinels
    //     (the P4 `runtime_schema_for` path). Introspection itself is lazy +
    //     deploy-keyed and runs on the first CRUD op, so marking readiness here is
    //     sufficient — we deliberately do NOT introspect (no catalog reads) at
    //     register time, matching the old fast/cheap registration boundary.
    //   * declared-schema cache — the dispatch caller also still calls
    //     `cache_schema`, so the declared-ONLY hints that introspection cannot
    //     recover keep working byte-identically: the `t.id(prefix)` typed-id
    //     `idPrefix` read by `system_fields_pass::prefix_for_collection`, and the
    //     `schema_for` hints consulted by vector-search / unmask / mask-drift.
    // Net on PG: the runtime never CREATEs/ALTERs schema; it only reads. If the
    // engine somehow has not applied the schema at deploy, runtime CRUD errors
    // normally ("column does not exist") — deploy ordering (§8) guarantees the
    // schema is present first; we deliberately do NOT re-add a runtime
    // auto-migrate fallback.
    //
    // On the SQLite dialect (dev tier) `registerModel` is UNCHANGED — it STILL
    // auto-creates/migrates from the declared `default.schema`, because the engine
    // has NO SQLite apply path yet (design §9 / §10 non-goal; risk R4). This is
    // the documented split: PG schema is engine-owned at deploy; SQLite dev keeps
    // the runtime auto-migrate until a future SQLite-engine phase. Consequently
    // `default.schema` / `installSchema` are now PG-UNUSED for DDL (PG runtime
    // metadata comes from introspection) but stay SQLite-CONSUMED right here; full
    // removal from the deploy contract is DEFERRED to that SQLite-engine phase
    // (design §5 / §12 P5) — do not gut the contract field this phase.
    match (backend.as_postgres(), backend.as_sqlite()) {
        // PG: NO runtime DDL — the engine (P6 deploy-apply) is the PG schema
        // authority. This path no-ops the apply; the dispatch caller stamps
        // readiness (`mark_model_registered`) + the declared cache (`cache_schema`)
        // on the returned `Ok(())`, preserving the metadata-readiness contract
        // above WITHOUT any CREATE/ALTER. `_pg` is bound only to select the arm.
        (Some(_pg), _) => Ok(()),
        // SQLite dev tier (P6b): drive the security-hardened migration engine
        // (journal / versioning / drift / 12-step rebuild / baseline / dev
        // auto-approve), NOT the retired bespoke `run_sqlite_pipeline`. `sqlite`
        // is the data-plane backend A; `run_sqlite_via_engine` constructs the
        // hardened migration backend B on the same app file, applies through the
        // engine, drops B, re-ATTACHes A, and bridges the CDC name-cache — all
        // inside this awaited body (the ordering barrier, §7b.5). `deploy_id` is
        // read inside it (from ZEROSHIP_DEPLOY_ID) for journal/audit grouping.
        (_, Some(sqlite)) => {
            sqlite_engine::run_sqlite_via_engine(sqlite, app_id, collection, schema, indexes).await
        }
        // Unknown / future backend surfaces a typed, SDK-visible error rather than
        // aborting the spawned compio task via an `.expect()` panic.
        _ => Err(DbError::backend_unsupported("register_model")),
    }
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
pub async fn run_pipeline<B: RegisterBackend + DialectBuilder + AuditWriter>(
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
#[cfg(feature = "test-helpers")]
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

/// **P5 test seam** — drive the PRODUCTION dialect dispatch
/// ([`exec_register_model`]) without the V8 lifecycle. The backend is read
/// from the per-isolate context (install it first via
/// `set_postgres_pool_for_tests` / `set_sqlite_backend_for_tests`), so this
/// exercises the EXACT PG-no-DDL vs SQLite-auto-migrate branch the cutover
/// introduces — unlike `exec_register_model_with_pool`, which bypasses the
/// dispatch and calls `run_pipeline` directly.
#[cfg(feature = "test-helpers")]
pub async fn exec_register_model_via_dispatch_for_tests(
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
) -> Result<(), DbError> {
    exec_register_model(app_id, collection, schema, indexes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sqlite::SqliteBackend;
    use crate::backend::SqlExecutor;
    use crate::broker::SubscriptionMessage;
    use serde_json::json;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::Duration;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(f)
    }

    async fn next_change(
        sub: &crate::broker::Subscription,
    ) -> std::rc::Rc<crate::broker::ChangeEvent> {
        for _ in 0..50 {
            if let Some(msg) = sub.pop() {
                match msg {
                    SubscriptionMessage::Change(ev) => return ev,
                    SubscriptionMessage::Resync => continue,
                    SubscriptionMessage::Closed => panic!("subscription closed unexpectedly"),
                }
            }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for broker change event");
    }

    /// **P6b CDC-bridge regression (rewritten for the engine path).** After a
    /// SQLite ADD COLUMN through `run_sqlite_via_engine`, connection A's CDC
    /// name cache must reflect the new column (the engine ran the DDL on the
    /// hardened backend B; the bridge invalidates A's cache). Pre-bridge a stale
    /// cache would synthesize positional `c<N>` fallback keys for the new column.
    ///
    /// This drives the REAL engine path (B applies → drop B → A re-ATTACHes → CDC
    /// invalidate), not the retired `apply_sqlite` shim, with backend A installed
    /// in the per-isolate context exactly as production does.
    #[test]
    fn sqlite_register_model_via_engine_refreshes_cdc_name_cache_after_add_column() {
        run(async {
            let dir = tempfile::tempdir().expect("create tempdir");
            let backend = Rc::new(
                SqliteBackend::new(PathBuf::from(dir.path())).expect("open backend"),
            );
            // Install A in the per-isolate context so `run_sqlite_via_engine`
            // resolves the same data-plane backend the CDC publisher reads.
            crate::set_sqlite_backend_for_tests(backend.clone());
            let app_id = "app_cdc_name_refresh";
            let collection = "messages";

            let schema_v1 = json!({
                "_meta": {"strictness": "lenient"},
                "title": {"type": "string", "required": true}
            });
            sqlite_engine::run_sqlite_via_engine(
                &backend,
                app_id,
                collection,
                &schema_v1,
                &json!([]),
            )
            .await
            .expect("engine registers the initial schema");

            let sub = crate::broker::subscribe(app_id, collection);

            // CRUD on A (the data plane) — the ATTACH from step 5 means A sees the
            // engine-created table. A write here fires CDC and primes the cache.
            backend
                .pool_exec(
                    r#"INSERT INTO "app_cdc_name_refresh"."messages" (id, title)
                       VALUES ('msg_1', 'hello')"#,
                    &[],
                )
                .await
                .expect("seed row for CDC cache prime (A sees the migrated table)");
            let first = next_change(&sub).await;
            assert!(
                first.new_tuple.contains_key("title"),
                "first CDC decode must resolve the original column names"
            );

            // v2: ADD COLUMN body, through the engine. The DDL runs on B; the
            // bridge invalidates A's CDC name cache for `messages`.
            let schema_v2 = json!({
                "_meta": {"strictness": "lenient"},
                "title": {"type": "string", "required": true},
                "body": {"type": "string"}
            });
            sqlite_engine::run_sqlite_via_engine(
                &backend,
                app_id,
                collection,
                &schema_v2,
                &json!([]),
            )
            .await
            .expect("engine applies the widened schema (ADD COLUMN body)");

            backend
                .pool_exec(
                    r#"INSERT INTO "app_cdc_name_refresh"."messages" (id, title, body)
                       VALUES ('msg_2', 'hello-again', 'fresh-body')"#,
                    &[],
                )
                .await
                .expect("insert row after ADD COLUMN (A sees the migrated column)");
            let second = next_change(&sub).await;

            assert!(
                second.new_tuple.contains_key("body"),
                "CDC decode must refresh the cached column names after the engine ADD COLUMN"
            );
            assert!(
                second.changed_columns.iter().any(|c| c == "body"),
                "changed_columns must include the newly-added column name after cache refresh"
            );
            assert!(
                !second.new_tuple.keys().any(|k| {
                    k.strip_prefix('c')
                        .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()))
                }),
                "stale cache would synthesize positional fallback keys: {:?}",
                second.new_tuple.keys().collect::<Vec<_>>()
            );
        });
    }
}
