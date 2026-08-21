//! `db.registerModel(collection, schema, indexes)` - schema registration.
//!
//! # What this does depends on the backend, and on PG it is not DDL
//!
//! Read this before the pipeline below, because the pipeline does not run on
//! the production backend:
//!
//! * **PostgreSQL - metadata only, NO DDL.** The apply arm is
//!   `(Some(_pg), _) => Ok(())`. `zeroship-migrate` is the sole PG schema
//!   authority and creates the schema at DEPLOY time, before the app serves.
//!   What the call still earns is metadata: the dispatch caller stamps
//!   readiness (`mark_model_registered`) and the declared cache
//!   (`cache_schema`) on the returned `Ok(())`. The PG CRUD passes read that
//!   cache for declared-only facets - `t.id(prefix)` idPrefix, encrypted and
//!   mask facets - which introspection cannot recover. Since the
//!   migration-first cutover the schema value originates from the bundled
//!   `RuntimeSchemaDescriptor`, injected as `globalThis.__zsRuntimeDescriptor`.
//!   So on PG this registers a model's metadata; it creates nothing.
//!
//! * **SQLite dev tier - also registers metadata and creates NOTHING**, the
//!   same as the PG arm. The dev server applies the committed migrations to the
//!   app file ahead of the worker (`sdks/vite-plugin/src/gen-types/dev-apply.ts`),
//!   so the schema already exists by the time a request arrives; the arm keeps
//!   only `ensure_app_schema` (the ATTACH the data plane needs), and the caller
//!   stamps `mark_model_registered` / `cache_schema` on its `Ok(())`.
//!
//!   THIS PARAGRAPH SAID THE OPPOSITE until 2026-08-10: that the arm "applies,
//!   but through the MIGRATION ENGINE", calling
//!   [`sqlite_engine::run_sqlite_via_engine`] at first `registerModel`. That is
//!   stale - it describes the pre-cutover arm, and it contradicted the comment
//!   on the arm itself further down this file, which has said "metadata only,
//!   NO DDL" since the cutover. Two records of one arm, disagreeing, with the
//!   summary one wrong: the reader who stops at the module doc gets the
//!   opposite of the behaviour.
//!
//!   Consequence worth knowing before editing here:
//!   [`sqlite_engine::run_sqlite_via_engine`] now has **no production call
//!   site** - every caller in the tree is a `#[cfg(test)]` test in this file
//!   or reaches it through
//!   [`apply_declared_schema_to_dev_sqlite_for_tests`], the `test-helpers`
//!   seam the SQLite integration suite uses to stand in for the dev server's
//!   apply-ahead step (re-enumerated 2026-08-20). A charter or engine failure
//!   reachable only through it still cannot break a running dev server, whose
//!   apply goes through the addon's `applyIrSqlite` instead. The proposal that
//!   drove the cutover lists this module under "Retirement"
//!   (`docs/proposals/2026-08-09-dev-sqlite-migration-apply-ahead-of-runtime.md`,
//!   risk 5); deleting it means giving the suite an envelope-replay apply
//!   first, and that is not settled here.
//!
//! So the name is wider than either backend's behaviour: on BOTH backends this
//! registers metadata and creates nothing. Reading it as "this creates my
//! tables" is wrong on either tier - a migration process does that, ahead of
//! the runtime.
//!
//! Not reachable from creator code either way: `env.db.registerModel` is
//! `undefined` inside a handler and `env.db.__platform` throws
//! `platform_internal_only` - see `tests/platform_fence.rs`.
//!
//! # The four-phase pipeline - TEST-ONLY, run by NEITHER runtime arm
//!
//! `bootstrap`, `plan`, `validate` and `apply` are each
//! `#[cfg(any(test, feature = "test-helpers"))]` (see the `mod` declarations
//! below); `sqlite_engine` is the only ungated one. So the phases compile for
//! this crate's tests and for downstream test targets, and for nothing else.
//! The PG arm no-ops and the SQLite arm goes through the engine, so no
//! production path executes them.
//!
//! They are still worth reading - the integration tests drive the stages
//! directly, and the shape is the reference for what an apply must do - but
//! read them as a tested design, not as the code serving a request.
//!
//! Proposal A2 (docs/archive/zeroship-db.md, section "A2. Deploy-time
//! data validation") defines the contract. It is accurate for the phases and
//! predates both cutovers above:
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

// NOT cfg-gated: the SQLite register arm calls `ensure_app_schema` in every
// build, so the trait must be in scope in the production build too. The
// `use` below is test-only — putting this there compiles under `cargo test`
// and fails under `cargo check`, which is how it was first written.
use crate::backend::NamespaceManager;
#[cfg(any(test, feature = "test-helpers"))]
use crate::backend::{AuditWriter, DialectBuilder, RegisterBackend};
use crate::context;
use crate::error::DbError;
use crate::v8_bridge::{runtime_state, setup_js_promise};

#[cfg(any(test, feature = "test-helpers"))]
pub(crate) mod apply;
#[cfg(any(test, feature = "test-helpers"))]
pub(crate) mod bootstrap;
#[cfg(any(test, feature = "test-helpers"))]
pub(crate) mod plan;
pub(crate) mod sqlite_engine;
#[cfg(any(test, feature = "test-helpers"))]
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
    declared_collections: Vec<String>,
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
        match exec_register_model(
            &app_id_owned,
            &collection_owned,
            &schema,
            &indexes,
            &declared_collections,
        )
        .await
        {
            Ok(()) => {
                crate::mark_model_registered(&app_id_owned, &collection_owned);
                // Cache the schema so the CRUD encryption
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
    declared_collections: &[String],
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

    // The cutover (dialect-conditional; do NOT brick SQLite dev). The
    // schema-authority split (`docs/proposals/2026-06-18-schema-authority-drizzle-
    // model-design.md` §6/§9/§12) makes `zeroship-migrate` the SOLE PG schema
    // applier: the `zeroship-migrated` service creates/migrates the per-app PG
    // schema (and provisions the migration/runtime roles) before go-live via
    // `POST /v1/apps/{id}/migrations/apply`. So on the PG dialect
    // `registerModel` STOPS being a schema authority — it issues NO runtime DDL
    // (no `bootstrap` create-schema / `plan` / `validate` / `apply`). This is what
    // eliminates the two-applier overlap (plugin-db + engine both running DDL
    // under disjoint advisory-lock namespaces).
    //
    // What the PG no-DDL path STILL guarantees — the "schema-ready + metadata-
    // available" contract the introspection cache depends on (design §6):
    //   * readiness — the dispatch caller (`register_model_dispatch`) marks
    //     `is_model_registered` on this `Ok(())`, which is the gate
    //     `crud::introspect_schema::runtime_schema_for` checks before sourcing
    //     per-collection metadata from LIVE introspection + the engine's sentinels
    //     (the `runtime_schema_for` path). Introspection itself is lazy +
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
    // On the SQLite dialect (dev tier) the same is true, and THIS COMMENT SAID
    // THE OPPOSITE until 2026-08-20: that `registerModel` "drives the SAME
    // hardened zeroship-migrate engine", applying at first-register on the
    // developer's own local file. That described the pre-cutover arm and
    // contradicted the arm's own comment twenty lines below. Since d84cbbd84
    // the dev server applies the committed migrations through the addon's
    // `applyIrSqlite` before it spawns the runtime, so BOTH dialects reach the
    // arms below with the schema already in place. `installSchema` is PG-UNUSED
    // and SQLite-UNUSED for DDL; on both it supplies only the declared metadata
    // the CRUD passes cache.
    match (backend.as_postgres(), backend.as_sqlite()) {
        // PG: NO runtime DDL — the engine (deploy-apply) is the PG schema
        // authority. This path no-ops the apply; the dispatch caller stamps
        // readiness (`mark_model_registered`) + the declared cache (`cache_schema`)
        // on the returned `Ok(())`, preserving the metadata-readiness contract
        // above WITHOUT any CREATE/ALTER. `_pg` is bound only to select the arm.
        //
        // **Migration-first cutover.** The `schema` value this arm
        // receives (and that the dispatch caller stamps into `cache_schema`)
        // now originates from the bundled `RuntimeSchemaDescriptor` (the
        // migration fold's runtime descriptor), not the old declared t.* object:
        // the runtime injects the descriptor as
        // `globalThis.__zsRuntimeDescriptor` and `installSchema` runs the
        // `registerModel` chain off it. So the declared-only hints the PG CRUD
        // passes read out of the cache (`t.id(prefix)` idPrefix, encrypted /
        // mask facets) come from the fold — higher fidelity than the old
        // declared object — while this arm stays a pure no-op (no DDL). The
        // descriptor path is PG/`.zship`-only; SQLite dev (below) still receives
        // the declared schema and diffs it against live state.
        (Some(_pg), _) => Ok(()),
        // SQLite dev tier — metadata only, NO DDL, exactly like the PG arm
        // above. The dev server applies the committed migrations to the app
        // file ahead of the worker (`sdks/vite-plugin/src/gen-types/dev-apply.ts`),
        // so by the time a request reaches here the schema already exists and
        // this call has nothing to create.
        //
        // WHY THE OLD ARM COULD NOT WORK. It drove the migration engine from
        // the DESCRIPTOR, and the descriptor already carries the seven injected
        // system columns (gen-types folds them in at emit). Re-injecting them
        // here made the collection collide with the platform's own columns:
        //
        //   sqlite engine: desired_snapshot failed: invalid descriptor:
        //   collection 'todos' declares field 'created_at', which collides with
        //   an injected policy column
        //
        // reproduced on examples/db-todos, whose migration declares no system
        // columns at all. The register also ran PER COLLECTION in registration
        // order against a partial union, so a foreign key resolved only if its
        // target happened to register first. Both failures are the same
        // category error — the runtime doing the migration's job — and both
        // disappear by construction once the apply happens ahead of time.
        //
        // What is still earned here: the dispatch caller stamps readiness
        // (`mark_model_registered`) and the declared cache (`cache_schema`) on
        // the returned `Ok(())`, and the CRUD paths read that cache for the
        // declared-only facets introspection cannot recover (`t.id(prefix)`
        // idPrefix, encrypted/mask facets). The ATTACH is kept explicitly: the
        // data plane cannot read the app file it never attached.
        (_, Some(sqlite)) => {
            let _ = (&schema, &indexes, &declared_collections);
            sqlite.ensure_app_schema(app_id).await
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
/// Generic over [`RegisterBackend`] (was concrete
/// `&PostgresBackend`). The PG pool's `get()` lives behind
/// [`crate::backend::PgLockManager::acquire_pooled_client_for_lock`]
/// now, so the `PooledClient<'p>` lifetime still threads through to
/// `apply` but no concrete-type leak remains in this signature. Open
/// Q5 resolution per `docs/archive/p0-implementation-plan.md`
/// §"PR 3" + §3 Q5 and `docs/archive/db-system-design.md` §7.
#[cfg(any(test, feature = "test-helpers"))]
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
/// [`crate::backend::PostgresBackend`] around the pool and delegates to
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

/// Test seam that drives the PRODUCTION dialect dispatch
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
    // No declared-set hint from this seam — pass empty, which makes the dev
    // SQLite drop pass treat every non-desired live table as a real drop
    // candidate (the single-collection-only behaviour, before the warm
    // multi-collection fix below). Tests that exercise the warm
    // multi-collection drop-suppression path call
    // `run_sqlite_via_engine` directly with an explicit declared set.
    exec_register_model(app_id, collection, schema, indexes, &[]).await
}

/// Apply a declared collection's schema to the dev SQLite app file AHEAD of the
/// runtime, standing in for what the dev server does before it spawns the worker.
///
/// # Why a test needs this at all
///
/// Since the 2026-08-10 cutover (d84cbbd84) `registerModel` creates nothing on
/// EITHER dialect. A test that boots the runtime and calls `env.db.<coll>` must
/// therefore get its table the way a real app does - from a migration process
/// that ran first - or it is asserting against a database no creator has. In dev
/// that process is `applyMigrationsToDevSqlite`
/// (`sdks/vite-plugin/src/gen-types/dev-apply.ts`), which replays the committed
/// `migrations/*.ts` envelopes into `<db_dir>/zs-<app_id>.sqlite` through the
/// addon's `applyIrSqlite` verb before the runtime process exists.
///
/// # What this reproduces, and what it does NOT
///
/// It drives the SAME engine, on the SAME two files, under the same confined
/// inject ceiling, and finishes before the caller boots the runtime - so the
/// table shape (seven system columns, `["id"]` PK, the three system indexes) and
/// the ordering are the dev tier's.
///
/// It does NOT reproduce the dev server's FRONT END. `applyIrSqlite` replays
/// authored migration-IR envelopes through `deploy_envelopes`; this plans a
/// declarative diff of the declared schema against live state. So a defect in
/// envelope lowering, journal versioning of authored migrations, or the recorder
/// is invisible here. It is the closest apply-ahead available in-process while
/// the Rust side has no envelope-replay entry point for SQLite.
#[cfg(feature = "test-helpers")]
pub async fn apply_declared_schema_to_dev_sqlite_for_tests(
    backend: &crate::backend::SqliteBackend,
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
) -> Result<(), DbError> {
    sqlite_engine::run_sqlite_via_engine(backend, app_id, collection, schema, indexes, &[]).await
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
    ) -> std::sync::Arc<crate::broker::ChangeEvent> {
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

    /// CDC-bridge regression test (rewritten for the engine path). After a
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
                &[collection.to_string()],
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
                &[collection.to_string()],
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

    /// True if `table` exists in the app's ATTACHed SQLite schema (the data-plane
    /// backend A view). Reads `sqlite_master` in the app's schema namespace.
    async fn table_exists(backend: &SqliteBackend, app_id: &str, table: &str) -> bool {
        let sql = format!(
            r#"SELECT name FROM "{app_id}".sqlite_master WHERE type='table' AND name='{table}'"#
        );
        let rows = backend.query_json(&sql, &[]).await.expect("query sqlite_master");
        !rows.is_empty()
    }

    /// **Regression guard — warm multi-collection boot must NOT fail closed.**
    ///
    /// A warm app file already holds tables `c1` + `c2` (registered by a prior
    /// isolate). A FRESH isolate then registers them one at a time (the
    /// install-schema.ts order), `c1` FIRST. When `c1` registers, the sibling
    /// cache is empty → the per-collection desired union is `{c1}`, but live is
    /// `{c1,c2}`. Pre-fix, `c2` was a live-only table with no `live_ownership`
    /// entry → the differ's fail-closed drop pass raised `DropOfUnownedTable` and
    /// `registerModel` REJECTED — the app broke on every warm boot of any 2+-
    /// collection schema.
    ///
    /// Post-fix: `c2` is in the FULL declared set `[c1, c2]`, so the drop pass
    /// hides it (not-yet-registered sibling) → NO error, `c2` is NOT dropped, and
    /// then registering `c2` is a clean no-op. Both tables stay usable.
    ///
    /// RED before the fix: the first phase-2 `run_sqlite_via_engine(c1)` returns
    /// `Err(DropOfUnownedTable)` and the `.expect(...)` panics.
    #[test]
    fn sqlite_warm_multi_collection_fresh_isolate_registers_c1_first_no_drop() {
        run(async {
            let dir = tempfile::tempdir().expect("create tempdir");
            let backend = Rc::new(
                SqliteBackend::new(PathBuf::from(dir.path())).expect("open backend"),
            );
            crate::set_sqlite_backend_for_tests(backend.clone());
            let app_id = "default";
            let declared = [c("c1"), c("c2")];

            let c1_schema = json!({"title": {"type": "string", "required": true}});
            let c2_schema = json!({"label": {"type": "string", "required": true}});

            // ---- Prior isolate: register both, warming the file with c1 + c2. ----
            sqlite_engine::run_sqlite_via_engine(&backend, app_id, "c1", &c1_schema, &json!([]), &declared)
                .await
                .expect("warm: register c1");
            crate::cache_schema_for_tests(app_id, "c1", c1_schema.clone());
            sqlite_engine::run_sqlite_via_engine(&backend, app_id, "c2", &c2_schema, &json!([]), &declared)
                .await
                .expect("warm: register c2");
            crate::cache_schema_for_tests(app_id, "c2", c2_schema.clone());

            assert!(table_exists(&backend, app_id, "c1").await, "warm c1 created");
            assert!(table_exists(&backend, app_id, "c2").await, "warm c2 created");

            // ---- Fresh isolate: empty sibling cache; register c1 FIRST. ----
            crate::simulate_fresh_isolate_for_tests(app_id, &["c1", "c2"]);

            sqlite_engine::run_sqlite_via_engine(&backend, app_id, "c1", &c1_schema, &json!([]), &declared)
                .await
                .expect("H1: fresh-isolate register of c1 first must NOT fail closed on the live c2 sibling");
            crate::cache_schema_for_tests(app_id, "c1", c1_schema.clone());

            // c2 must survive the c1 register (it is declared, just not yet
            // re-registered on this isolate).
            assert!(
                table_exists(&backend, app_id, "c2").await,
                "H1: c2 must NOT be dropped when c1 registers first on a warm file"
            );

            // Then c2 re-registers cleanly (no-op against the warm table).
            sqlite_engine::run_sqlite_via_engine(&backend, app_id, "c2", &c2_schema, &json!([]), &declared)
                .await
                .expect("H1: re-register c2 must be a clean no-op");

            assert!(table_exists(&backend, app_id, "c1").await, "c1 still usable");
            assert!(table_exists(&backend, app_id, "c2").await, "c2 still usable");
        });
    }

    /// **Over-suppression guard — a GENUINELY-removed collection still drops.**
    ///
    /// Warm file holds `c1` + `c2`. The app's schema is then edited to declare
    /// ONLY `c1` (c2 removed). A fresh isolate registers `c1` with the FULL
    /// declared set `[c1]` (c2 is NOT in it). The drop pass must now author the
    /// owned drop of `c2` — confirming the warm-boot fix above did not
    /// over-suppress real removals.
    ///
    /// RED before the fix: pre-fix `c2` had no `live_ownership` entry, so this
    /// path raised `DropOfUnownedTable` instead of dropping (`.expect` panics);
    /// the assertion that c2 is gone could never be reached.
    #[test]
    fn sqlite_genuinely_removed_collection_is_dropped() {
        run(async {
            let dir = tempfile::tempdir().expect("create tempdir");
            let backend = Rc::new(
                SqliteBackend::new(PathBuf::from(dir.path())).expect("open backend"),
            );
            crate::set_sqlite_backend_for_tests(backend.clone());
            let app_id = "default";

            let c1_schema = json!({"title": {"type": "string", "required": true}});
            let c2_schema = json!({"label": {"type": "string", "required": true}});

            // Warm the file with both.
            let both = [c("c1"), c("c2")];
            sqlite_engine::run_sqlite_via_engine(&backend, app_id, "c1", &c1_schema, &json!([]), &both)
                .await
                .expect("warm: register c1");
            crate::cache_schema_for_tests(app_id, "c1", c1_schema.clone());
            sqlite_engine::run_sqlite_via_engine(&backend, app_id, "c2", &c2_schema, &json!([]), &both)
                .await
                .expect("warm: register c2");
            crate::cache_schema_for_tests(app_id, "c2", c2_schema.clone());
            assert!(table_exists(&backend, app_id, "c2").await, "warm c2 created");

            // Fresh isolate; the new declared schema has ONLY c1 (c2 removed).
            crate::simulate_fresh_isolate_for_tests(app_id, &["c1", "c2"]);
            let only_c1 = [c("c1")];
            sqlite_engine::run_sqlite_via_engine(&backend, app_id, "c1", &c1_schema, &json!([]), &only_c1)
                .await
                .expect("register c1 with c2 removed from the declared set");

            assert!(table_exists(&backend, app_id, "c1").await, "c1 still present");
            assert!(
                !table_exists(&backend, app_id, "c2").await,
                "H1 guard: a collection genuinely removed from the declared schema MUST be dropped"
            );
        });
    }

    fn c(s: &str) -> String {
        s.to_string()
    }
}
