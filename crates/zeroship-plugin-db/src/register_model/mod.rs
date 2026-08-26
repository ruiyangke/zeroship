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
//!   THIS PARAGRAPH SAID THE OPPOSITE until 2026-08-10: that the arm applied
//!   through the MIGRATION ENGINE at first `registerModel`. That described the
//!   pre-cutover arm and contradicted the comment on the arm itself further down
//!   this file, which has said "metadata only, NO DDL" since the cutover.
//!
//!   The engine-driven arm it described is GONE from this crate. It had no
//!   production call site: every caller was a test or the `test-helpers` seam
//!   standing in for the dev server's apply-ahead. That fixture now lives in
//!   `tests/support/sqlite_apply_ahead.rs`, where the engine is a dev-dependency,
//!   so a test can keep it without the LIBRARY carrying a migration engine it
//!   never calls. `crates/zeroship-plugin-db` now names `zeroship-migrate`
//!   nowhere in `src/`.
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
//! below), and since the engine arm was removed there is no ungated one left.
//! So the phases compile for this crate's tests and for downstream test targets,
//! and for nothing else. Both arms register metadata and create nothing, so no
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

/// Lazily initialise the pool, resolve the deploy id, then run the register
/// step through the backend.
///
/// `collection` is unread here for the same reason it is unread in
/// [`run_pipeline`]: registering is per-app now, not per-collection.
async fn exec_register_model(
    app_id: &str,
    _collection: &str,
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
