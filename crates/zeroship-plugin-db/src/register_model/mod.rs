//! `db.registerModel(collection, schema, indexes)` - schema REGISTRATION.
//!
//! It registers. It does not create, alter, or reconcile anything.
//!
//! # What each backend does
//!
//! * **PostgreSQL: nothing.** The arm is `Ok(())`. `zeroship-migrate` is the
//!   sole PG schema authority and applies at DEPLOY, before the app serves.
//! * **SQLite: one ATTACH.** `SqliteBackend::attach_app_file` binds
//!   `zs-<app_id>.sqlite` into this worker's session under the `<app_id>`
//!   alias. It is NOT schema creation - the file and its tables come from the
//!   vite dev-server's apply-ahead
//!   (`sdks/vite-plugin/src/gen-types/dev-apply.ts`).
//!
//! Both arms then get the same two effects from the dispatch caller, on `Ok`:
//! `mark_model_registered` (a per-thread flag) and `cache_schema` (the
//! descriptor entry, keyed by app-at-deploy). The second one is what makes the
//! collection SERVEABLE at all: `crate::descriptor::collection_schema` is the
//! data plane's sole schema authority, and a collection with no entry is
//! refused with `collection_not_declared` on every read and every write.
//!
//! # The ATTACH is in the wrong place, and that is a known item
//!
//! This module holding the only production `attach_app_file` call means SQLite
//! CRUD depends on registerModel having run first. A session that
//! never registered has no alias attached, and nothing re-attaches on its own -
//! `sqlite::session`'s recovery error says as much in plain text.
//!
//! It belongs in the data plane, which is where the file is used. It is not
//! there yet because there is no single chokepoint to put it: `exec.rs`
//! resolves `route.app_id()` per operation and the backend's exec methods never
//! receive an `app_id` (it is interpolated into the SQL). The contained fix is
//! attach-and-retry inside `SqliteSession` on an "unknown database" error,
//! which touches no signature and no PG path.
//!
//! # A four-phase pipeline used to live here
//!
//! Four sibling modules introspected the live catalog, diffed it against the
//! declared schema, and applied the difference as DDL. All four are DELETED,
//! names included, so that a search for them turns up nothing rather than this
//! paragraph. They were `#[cfg(any(test, feature = "test-helpers"))]`
//! and no production build ever contained them, so reading a call chain from
//! them into `pool_exec_ddl` and concluding plugin-db applied schema at deploy
//! was a mistake the gating made easy - the chain was real and unreachable.
//!
//! Nothing consumed their output either: the data plane sources column types,
//! encryption and mask metadata from the RUNTIME DESCRIPTOR
//! (`crud::read_pipeline`, `crud::write_pipeline`, both through
//! `crate::descriptor`). The diff was a second model of schema truth competing
//! with the one that is actually read.
//!
//! # Not reachable from creator code
//!
//! `env.db.registerModel` is `undefined` inside a handler and
//! `env.db.__platform` throws `platform_internal_only` - see
//! `tests/platform_fence.rs`.

use serde_json::Value;
use zeroship_runtime::state::{OpResult, ResolveValue};

use crate::binding::DbBinding;
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
    binding: &DbBinding,
    collection: &str,
    schema: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let app_id = binding.app_id();

    // Fast path: already registered on this thread.
    if crate::is_model_registered(app_id, collection) {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let undefined = v8::undefined(scope);
        resolver.resolve(scope, undefined.into());
        return promise;
    }

    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    let binding_owned = binding.clone();
    let collection_owned = collection.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_register_model(binding_owned.app_id()).await
        {
            Ok(()) => {
                crate::mark_model_registered(binding_owned.app_id(), &collection_owned);
                // Install this collection's descriptor entry under the
                // APP-AT-DEPLOY binding, so a worker thread holding a pinned
                // and a current isolate of one app never serves one deploy's
                // schema to the other. Cloned because the closure captures
                // `schema` by move; the store is per-isolate and lives for the
                // isolate's lifetime (no eviction).
                context::with_mut(|c| {
                    c.cache_schema(&binding_owned, &collection_owned, schema.clone());
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

/// The database half of registerModel. It needs ONE argument.
///
/// Lazily initialises the pool, then dispatches on the backend: PostgreSQL
/// returns `Ok(())`, SQLite attaches the app file. Registration is per-APP,
/// not per-collection, so `collection` is not among the arguments - and
/// neither are the declared schema, the index list or the declared-collection
/// set, none of which reach the database at all.
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

    // NEITHER ARM APPLIES SCHEMA, and the reason is the same on both: something
    // else already did, before this code could run.
    //
    // On PostgreSQL the `zeroship-migrated` service creates and migrates the
    // per-app schema (and provisions the migration/runtime roles) before
    // go-live, via `POST /v1/apps/{id}/migrations/apply`. On SQLite the vite
    // dev server applies the committed migrations to the app file before it
    // spawns the runtime. So `registerModel` reaches a schema that is already
    // in place, on both dialects, and issuing DDL here could only conflict with
    // the authority that owns it.
    //
    // What registration still earns comes from the dispatch caller reacting to
    // this `Ok(())`, not from here:
    //
    //   * readiness. `mark_model_registered` sets the per-thread flag the warm
    //     short-circuit at the top of `register_model_dispatch` reads.
    //   * THE SCHEMA ITSELF. `cache_schema` installs this collection's
    //     descriptor entry into the per-isolate store, and
    //     `crate::descriptor::collection_schema` reads nothing else. Column
    //     types, `encrypted` mode/keyId/wraps, `mask` kind/classification, the
    //     `t.id(prefix)` typed-id `idPrefix`, `vectorDims` and the mask
    //     sibling's `storage.valueColumn` all arrive on this one path. This
    //     function deliberately reads no catalog, keeping registration cheap.
    //
    // If an authority somehow has not applied the schema, runtime CRUD fails
    // normally ("column does not exist"). That is the intended behaviour: there
    // is deliberately no runtime auto-migrate fallback, because a fallback is a
    // second applier, and two appliers under disjoint locks is what this
    // arrangement exists to prevent.
    match (backend.as_postgres(), backend.as_sqlite()) {
        // PG: nothing to do. `_pg` is bound only to select the arm.
        //
        // On the `.zship` path the `schema` the dispatch caller caches
        // originates from the bundled `RuntimeSchemaDescriptor` that the
        // runtime injects as `globalThis.__zsRuntimeDescriptor`, off which
        // `installSchema` runs the `registerModel` chain. So the declared-only
        // facets the PG CRUD passes read back out of the cache come from the
        // migration fold rather than from a hand-declared object, which is
        // higher fidelity. It changes what is cached, not what this arm does.
        (Some(_pg), _) => Ok(()),
        // SQLite: the ATTACH, and nothing else. The data plane cannot read an
        // app file it never attached.
        //
        // WHY THIS ARM DOES NOT APPLY SCHEMA, beyond the ordering argument
        // above: when it did, it drove the migration engine from the
        // descriptor, and the descriptor already carries the seven injected
        // system columns (gen-types folds them in at emit). Re-injecting them
        // collided with the platform's own columns:
        //
        //   sqlite engine: desired_snapshot failed: invalid descriptor:
        //   collection 'todos' declares field 'created_at', which collides with
        //   an injected policy column
        //
        // reproduced on examples/db-todos, whose migration declares no system
        // columns at all. It also ran PER COLLECTION in registration order
        // against a partial union, so a foreign key resolved only if its target
        // happened to register first. Both are the same category error, the
        // runtime doing the migration's job, and both disappear by construction
        // once the apply happens ahead of time.
        (_, Some(sqlite)) => sqlite.attach_app_file(app_id).await,
        // Unknown / future backend surfaces a typed, SDK-visible error rather than
        // aborting the spawned compio task via an `.expect()` panic.
        _ => Err(DbError::backend_unsupported("register_model")),
    }
}



/// Test seam that drives the PRODUCTION dialect dispatch
/// ([`exec_register_model`]) without the V8 lifecycle.
///
/// The backend is read from the per-isolate context, so install one first via
/// `set_postgres_pool_for_tests` / `set_sqlite_backend_for_tests`. Which arm
/// runs is then decided by exactly the match production uses, not by a
/// test-side copy of it.
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
    // whose collection must be serveable at all (so `collection_schema`
    // resolves and the encryption / mask stages apply) calls
    // `mark_model_registered_for_tests` / `cache_schema_for_tests` itself. The
    // existing callers do exactly that.
    let _ = (collection, schema, indexes);
    exec_register_model(app_id).await
}
