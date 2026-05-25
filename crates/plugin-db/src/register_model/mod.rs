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

use crate::backend::{
    AuditWriter, DialectBuilder, FullTextIndex, IndexBuilder, LockScope, RegisterBackend,
    SpatialIndex, SqliteBackend, VectorIndex,
};
use crate::context;
use crate::diff::{ChangeClass, ChangeKind};
use crate::error::DbError;
use crate::query::IndexKind;
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
    let deploy_id =
        std::env::var("ZEROSHIP_DEPLOY_ID").unwrap_or_else(|_| "cold_start".to_string());

    match (backend.as_postgres(), backend.as_sqlite()) {
        (Some(pg), _) => run_pipeline(pg, app_id, collection, schema, indexes, &deploy_id).await,
        (_, Some(sqlite)) => {
            run_sqlite_pipeline(sqlite, app_id, collection, schema, indexes, &deploy_id).await
        }
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

async fn run_sqlite_pipeline(
    backend: &SqliteBackend,
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
    deploy_id: &str,
) -> Result<(), DbError> {
    crate::cross_app_fk::reject_cross_app_fk(schema, app_id)?;

    let strictness = schema
        .get("_meta")
        .and_then(|m| m.get("strictness"))
        .and_then(Value::as_str)
        .unwrap_or("strict")
        .to_string();

    let scope = LockScope::GlobalApp {
        app_id: app_id.to_string(),
        name: bootstrap::LOCK_TAG.to_string(),
    };
    let _lock_guard = crate::backend::sqlite::lock::SqliteLockGuard::acquire(backend, &scope).await?;

    let ctx = match bootstrap::build_ctx(
        backend,
        app_id,
        collection,
        schema,
        indexes,
        deploy_id,
        strictness,
    )
    .await
    {
        Ok(ctx) => ctx,
        Err(e) => return Err(e),
    };

    let approved_res = match plan::compute_plan(backend, &ctx, collection, schema).await {
        Ok(plan) => validate::validate(backend, &ctx, plan)
            .await
            .map_err(|envelope_json| DbError::SchemaRefused {
                code: "validation_refused",
                envelope_json,
            }),
        Err(e) => Err(e),
    };
    let approved = match approved_res {
        Ok(approved) => approved,
        Err(e) => return Err(e),
    };

    apply_sqlite(backend, &ctx, &approved).await
}

async fn apply_sqlite(
    backend: &SqliteBackend,
    ctx: &bootstrap::RegisterContext,
    approved: &validate::ApprovedPlan,
) -> Result<(), DbError> {
    for op in &approved.ops {
        if op.class == ChangeClass::Destructive {
            continue;
        }

        let audit_id = match backend
            .write_audit_row_returning_id(
                &ctx.app_id,
                &crate::audit::AuditRow {
                    collection: op.collection.clone(),
                    phase: crate::audit::Phase::Ddl,
                    change_class: op.class.as_audit(),
                    change_kind: op.change_kind.as_sql().to_string(),
                    details: op.details.clone(),
                    ddl_sql: op.sql.clone(),
                    status: crate::audit::InitialStatus::Running,
                    deploy_id: ctx.deploy_id.clone(),
                    schema_version: ctx.schema_version,
                    actor: crate::audit::ActorKind::Auto,
                },
            )
            .await
        {
            Ok(id) => Some(id),
            Err(audit_err) => {
                tracing::warn!(
                    app_id = %ctx.app_id,
                    collection = %op.collection,
                    transition = "Running/insert_failed",
                    audit_err = %audit_err,
                    "audit: failed to insert running row",
                );
                None
            }
        };

        let result = match &op.change_kind {
            ChangeKind::CreateTable
            | ChangeKind::AddColumn
            | ChangeKind::AddForeignKey
            | ChangeKind::DropForeignKey => {
                if let Some(sql) = &op.sql {
                    backend.exec_batch(sql).await
                } else {
                    Ok(())
                }
            }
            ChangeKind::AddIndex => {
                let spec = ctx
                    .declared_indexes
                    .iter()
                    .find(|s| {
                        op.details.get("index_name").and_then(Value::as_str)
                            == Some(s.name.as_str())
                    })
                    .cloned();
                if let Some(spec) = spec {
                    match &spec.kind {
                        IndexKind::BTree => {
                            backend
                                .create_index_with_recovery(
                                    &ctx.app_id,
                                    &op.collection,
                                    &spec,
                                    &ctx.deploy_id,
                                    ctx.schema_version,
                                )
                                .await
                        }
                        IndexKind::Vector { dims, metric } => {
                            let column = spec.columns.first().map(String::as_str).unwrap_or("");
                            backend
                                .ensure_vector_index(
                                    &ctx.app_id,
                                    &op.collection,
                                    column,
                                    *dims,
                                    *metric,
                                )
                                .await
                        }
                        IndexKind::Fts { language } => {
                            backend
                                .ensure_fts_index(
                                    &ctx.app_id,
                                    &op.collection,
                                    &spec.columns,
                                    language,
                                )
                                .await
                        }
                        IndexKind::Spatial => {
                            let column = spec.columns.first().map(String::as_str).unwrap_or("");
                            backend
                                .ensure_spatial_index(&ctx.app_id, &op.collection, column)
                                .await
                        }
                    }
                } else {
                    Ok(())
                }
            }
            ChangeKind::MaskBackfill { .. }
            | ChangeKind::MaskRewrite { .. }
            | ChangeKind::MaskRemove { .. } => Err(DbError::backend_unsupported("register_model")),
            ChangeKind::DropColumn | ChangeKind::DropIndex => continue,
        };

        if result.is_ok() && refreshes_sqlite_cdc_name_cache(&op.change_kind) {
            backend.invalidate_cdc_name_cache(&ctx.app_id, &op.collection);
        }

        if let Some(id) = audit_id {
            match &result {
                Ok(_) => {
                    if let Err(audit_err) = backend
                        .update_audit_status(
                            &ctx.app_id,
                            id,
                            crate::audit::TerminalStatus::Applied,
                            None,
                        )
                        .await
                    {
                        tracing::warn!(
                            app_id = %ctx.app_id,
                            audit_id = id,
                            transition = "Applied",
                            audit_err = %audit_err,
                            "update_audit_status failed; row stays in 'running' until reset",
                        );
                    }
                }
                Err(e) => {
                    let msg = e.clone().into_string();
                    if let Err(audit_err) = backend
                        .update_audit_status(
                            &ctx.app_id,
                            id,
                            crate::audit::TerminalStatus::Failed,
                            Some(msg.as_str()),
                        )
                        .await
                    {
                        tracing::warn!(
                            app_id = %ctx.app_id,
                            audit_id = id,
                            transition = "Failed",
                            ddl_err = %msg,
                            audit_err = %audit_err,
                            "update_audit_status failed; row stays in 'running' until reset",
                        );
                    }
                }
            }
        }

        result?;
    }

    Ok(())
}

fn refreshes_sqlite_cdc_name_cache(change_kind: &ChangeKind) -> bool {
    matches!(change_kind, ChangeKind::CreateTable | ChangeKind::AddColumn)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sqlite::SqliteBackend;
    use crate::backend::SqlExecutor;
    use crate::broker::SubscriptionMessage;
    use serde_json::json;
    use std::path::PathBuf;
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

    #[test]
    fn sqlite_register_model_refreshes_cdc_column_name_cache_after_add_column() {
        run(async {
            let dir = tempfile::tempdir().expect("create tempdir");
            let backend = SqliteBackend::new(PathBuf::from(dir.path())).expect("open backend");
            let app_id = "app_cdc_name_refresh";
            let collection = "messages";

            let schema_v1 = json!({
                "_meta": {"strictness": "lenient"},
                "title": {"type": "string", "required": true}
            });
            run_sqlite_pipeline(
                &backend,
                app_id,
                collection,
                &schema_v1,
                &json!([]),
                "deploy_v1",
            )
            .await
            .expect("register initial schema");

            let sub = crate::broker::subscribe(app_id, collection);

            backend
                .pool_exec(
                    r#"INSERT INTO "app_cdc_name_refresh"."messages" (id, title)
                       VALUES ('msg_1', 'hello')"#,
                    &[],
                )
                .await
                .expect("seed row for CDC cache prime");
            let first = next_change(&sub).await;
            assert!(
                first.new_tuple.contains_key("title"),
                "first CDC decode must resolve the original column names"
            );

            let schema_v2 = json!({
                "_meta": {"strictness": "lenient"},
                "title": {"type": "string", "required": true},
                "body": {"type": "string"}
            });
            let ctx = bootstrap::RegisterContext {
                app_id: app_id.to_string(),
                deploy_id: "deploy_v2".to_string(),
                schema_version: 2,
                strictness: "lenient".to_string(),
                declared_indexes: Vec::new(),
                collection: collection.to_string(),
                schema_json: schema_v2.clone(),
            };
            let approved = validate::ApprovedPlan {
                ops: vec![crate::diff::DiffOp {
                    collection: collection.to_string(),
                    change_kind: ChangeKind::AddColumn,
                    class: ChangeClass::Additive,
                    sql: Some(
                        r#"ALTER TABLE "app_cdc_name_refresh"."messages" ADD COLUMN "body" TEXT"#
                            .to_string(),
                    ),
                    details: json!({
                        "kind": "add_column",
                        "field": "body",
                    }),
                    field: Some("body".to_string()),
                }],
            };
            apply_sqlite(
                &backend,
                &ctx,
                &approved,
            )
            .await
            .expect("apply widened schema");

            backend
                .pool_exec(
                    r#"INSERT INTO "app_cdc_name_refresh"."messages" (id, title, body)
                       VALUES ('msg_2', 'hello-again', 'fresh-body')"#,
                    &[],
                )
                .await
                .expect("insert row after ADD COLUMN");
            let second = next_change(&sub).await;

            assert!(
                second.new_tuple.contains_key("body"),
                "CDC decode must refresh the cached column names after register_model ADD COLUMN"
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
