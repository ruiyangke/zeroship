//! `db.registerModel(collection, schema, indexes)` - schema REGISTRATION.
//!
//! It registers. It does not create, alter, or reconcile anything.
//!
//! # What each backend does
//!
//! * **PostgreSQL: nothing.** The arm is `Ok(())`. `zeroship-migrate` is the
//!   sole PG schema authority and applies at DEPLOY, before the app serves.
//! * **SQLite: one ATTACH.** `ensure_app_schema` binds `zs-<app_id>.sqlite`
//!   into this worker's session under the `<app_id>` alias. It is NOT schema
//!   creation - the file and its tables come from the vite dev-server's
//!   apply-ahead. The name is inherited from the PG side, where the same trait
//!   method IS `CREATE SCHEMA IF NOT EXISTS`, and it misleads on SQLite.
//!
//! Both arms then get the same two effects from the dispatch caller, on `Ok`:
//! `mark_model_registered` (a per-thread flag) and `cache_schema` (the declared
//! JSON). The flag is load-bearing beyond the fast path below:
//! `crud::introspect_schema::runtime_schema_for` returns `None` for an
//! UNREGISTERED collection, so registration is what turns introspected
//! encryption / mask metadata on for a collection's reads and writes.
//!
//! # The ATTACH is in the wrong place, and that is a known item
//!
//! `register_model/mod.rs` holding the only production `ensure_app_schema` call
//! means SQLite CRUD depends on registerModel having run first. A session that
//! never registered has no alias attached, and nothing re-attaches on its own -
//! `sqlite::session`'s recovery error says as much in plain text.
//!
//! It belongs in the data plane, which is where the file is used. It is not
//! there yet because there is no single chokepoint to put it: `exec.rs`
//! resolves `route.app_id()` per operation, the backend's exec methods never
//! receive an `app_id` (it is interpolated into the SQL), and
//! `runtime_schema_for` short-circuits before any DB work for the very case
//! that needs it. The contained fix is attach-and-retry inside `SqliteSession`
//! on an "unknown database" error, which touches no signature and no PG path.
//!
//! # A four-phase pipeline used to live here
//!
//! `bootstrap`, `plan`, `validate` and `apply` introspected the live catalog,
//! diffed it against the declared schema, and applied the difference as DDL.
//! All four are DELETED. They were `#[cfg(any(test, feature = "test-helpers"))]`
//! and no production build ever contained them, so reading a call chain from
//! them into `pool_exec_ddl` and concluding plugin-db applied schema at deploy
//! was a mistake the gating made easy - the chain was real and unreachable.
//!
//! Nothing consumed their output either: the data plane sources column types,
//! encryption and mask metadata from LIVE introspection plus the engine's
//! sentinels (`crud::read_pipeline`, `crud::write_pipeline`), never from the
//! declared schema. The diff was a second model of schema truth competing with
//! the introspection that is actually read.
//!
//! # Not reachable from creator code
//!
//! `env.db.registerModel` is `undefined` inside a handler and
//! `env.db.__platform` throws `platform_internal_only` - see
//! `tests/platform_fence.rs`.

use serde_json::Value;
use zeroship_runtime::state::{OpResult, ResolveValue};

// NOT cfg-gated: the SQLite register arm calls `ensure_app_schema` in every
// build, so the trait must be in scope in the production build too. The
// `use` below is test-only — putting this there compiles under `cargo test`
// and fails under `cargo check`, which is how it was first written.

use crate::context;
use crate::error::DbError;
use crate::v8_bridge::{runtime_state, setup_js_promise};


/// `zeroship.db.registerModel(collection, schemaJson)` -> `Promise<void>`
///
/// Records that `collection` exists and caches its declared schema. It creates
/// nothing: the table must already have been made by the schema authority.
/// Idempotent, and safe to call on every cold start.
pub fn register_model_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    schema: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);

    // Fast path: already registered on this thread.
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
        match exec_register_model(&app_id_owned).await
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
/// The database half of registerModel. It needs ONE argument.
///
/// The dispatch signature carries `collection`, `schema`, `indexes` and the
/// declared-collection set because the JS API does, and because the dispatch
/// caller genuinely uses `schema` - it seeds the declared cache. None of them
/// reach the database: on PostgreSQL this returns `Ok(())`, and on SQLite it
/// attaches the app file, which is keyed by `app_id` alone.
///
/// The four dead parameters used to be accepted here and discarded with a
/// `let _ = (...)`, which read like they were pending rather than irrelevant.
async fn exec_register_model(app_id: &str) -> Result<(), DbError> {
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
            sqlite.attach_app_file(app_id).await
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
    // THE THREE EXTRA ARGUMENTS DO NOTHING, and the signature keeps them only
    // so a test reads like the JS call it stands in for. `exec_register_model`
    // takes `app_id` alone; see its doc.
    //
    // This seam is the DATABASE half. It does NOT `mark_model_registered` or
    // `cache_schema` - those happen in `register_model_dispatch`, above. A test
    // that needs a collection to count as registered (so `runtime_schema_for`
    // stops returning `None` and encryption / mask metadata applies) calls
    // `mark_model_registered_for_tests` / `cache_schema_for_tests` itself. The
    // existing callers do exactly that.
    let _ = (collection, schema, indexes);
    exec_register_model(app_id).await
}
