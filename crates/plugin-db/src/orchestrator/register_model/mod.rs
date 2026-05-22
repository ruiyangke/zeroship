//! `db.registerModel(collection, schema, indexes)` — the four-phase DDL
//! pipeline.
//!
//! Proposal A2 (docs/proposals/zeroship-db.md) defines the contract:
//!
//! 1. **Bootstrap** ([`bootstrap`]) — create the per-app Postgres schema,
//!    the `__zeroship_migrations` audit table (idempotent), acquire the
//!    session-scoped advisory lock, compute the deploy's `schema_version`,
//!    expand declared + named indexes. Returns a [`RegisterContext`] the
//!    later stages thread through.
//! 2. **Plan** ([`plan`]) — introspect `pg_catalog`, diff against the
//!    declared schema, classify each change as additive / compatible /
//!    destructive. Returns a [`Plan`].
//! 3. **Validate** ([`validate`]) — apply safety rules. Destructive ops
//!    under `strictness=strict` produce a `validation_refused` envelope
//!    the SDK consumes verbatim. Lenient deploys log + skip; off
//!    proceeds.
//! 4. **Apply** ([`apply`]) — two-pass DDL execution under the advisory
//!    lock (transactional ops first; `CREATE INDEX CONCURRENTLY` after
//!    releasing the lock). Every op writes an audit row through
//!    [`crate::audit`].
//!
//! Each submodule is `pub(crate)` so integration tests can call into the
//! stages independently. The V8-facing surface is
//! [`register_model_dispatch`] — unchanged from before the split.

use serde_json::Value;
use zeroship_runtime::state::OpResult;

use crate::context;
use crate::v8_bridge::{runtime_state, setup_promise};

pub(crate) mod apply;
pub(crate) mod bootstrap;
pub(crate) mod plan;
pub(crate) mod validate;

/// `zeroship.db.registerModel(collection, schemaJson)` → Promise<void>
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

    let (op_id, request_id, promise) = setup_promise(scope, &state);
    let app_id_owned = app_id.to_string();
    let collection_owned = collection.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_register_model(&app_id_owned, &collection_owned, &schema, &indexes).await {
            Ok(()) => {
                crate::mark_model_registered(&app_id_owned, &collection_owned);
                OpResult::Completed {
                    op_id,
                    value: "null".to_string(),
                    request_id,
                }
            }
            Err(e) => OpResult::Failed {
                op_id,
                error: e,
                request_id,
            },
        }
    }));

    promise
}

/// Lazily initialise the pool, resolve the deploy id, then drive the
/// four-phase pipeline.
async fn exec_register_model(
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
) -> Result<(), String> {
    // Lazy pool init
    let has_pool = context::with(|c| c.pool_initialised());
    if !has_pool {
        crate::init_pool_async()
            .await
            .map_err(|e| format!("db: lazy init failed: {e}"))?;
    }

    let pool = context::with(|c| c.pool())
        .ok_or_else(|| "db: pool not initialized".to_string())?;

    let deploy_id =
        std::env::var("ZEROSHIP_DEPLOY_ID").unwrap_or_else(|_| "cold_start".to_string());

    exec_register_model_with_pool(&pool, app_id, collection, schema, indexes, &deploy_id).await
}

/// Pool-driven variant of `exec_register_model`. Public so integration
/// tests can drive the four-phase orchestrator without going through V8.
///
/// `deploy_id` controls audit-log grouping (proposal A3 line 233 reserves
/// `'cold_start'` for pre-deploy DDL).
///
/// The four stages are:
///
/// ```text
/// bootstrap → plan → validate → apply
/// ```
///
/// Each stage is implemented as a free function in its own submodule;
/// this entry just sequences them. The advisory lock is held by the
/// `RegisterContext` returned from `bootstrap`; `apply` releases it
/// after pass 1 before running `CREATE INDEX CONCURRENTLY`.
pub async fn exec_register_model_with_pool(
    pool: &compio_postgres::Pool,
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
    deploy_id: &str,
) -> Result<(), String> {
    // 1. Bootstrap — schema, audit table, advisory lock, schema_version,
    //    expanded index specs. The returned context owns the lock_client
    //    so apply can release the lock between passes.
    let ctx = bootstrap::bootstrap(pool, app_id, collection, schema, indexes, deploy_id).await?;

    // 2. Plan — introspect live schema, diff against declared schema,
    //    classify each op.
    let plan = plan::compute_plan(pool, &ctx, collection, schema).await?;

    // 3. Validate — refuse destructive ops under `strict`, audit them
    //    either way so operators can see what was refused.
    let approved = validate::validate(pool, &ctx, plan).await?;

    // 4. Apply — execute the validated ops under the lock (pass 1) then
    //    release and run CIC unlocked (pass 2). Each op writes an audit
    //    row.
    apply::apply(pool, ctx, approved).await
}
