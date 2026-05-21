//! `db.registerModel(collection, schema, indexes)` — the four-phase DDL
//! pipeline.
//!
//! Proposal A2 (docs/proposals/zeroship-db.md) defines the contract:
//!
//! 1. **Bootstrap** — create the per-app Postgres schema and the
//!    `__zeroship_migrations` audit table (idempotent).
//! 2. **Diff** — introspect `pg_catalog`, classify each declared
//!    change as additive / compatible / destructive.
//! 3. **Classify / validate** — refuse destructive changes when
//!    strictness=strict; lenient deploys log + skip.
//! 4. **Apply** — pass 1 (transactional DDL under an advisory lock),
//!    pass 2 (`CREATE INDEX CONCURRENTLY` after releasing the lock).
//!
//! Every DDL op writes a `__zeroship_migrations` row through
//! [`crate::audit`]. The audited create-index recovery loop
//! ([`create_index_with_recovery_audited`]) handles the
//! INVALID-index landing case and emits a structured
//! `validation_refused` envelope when the data violates a new
//! constraint.

use std::rc::Rc;

use serde_json::Value;
use zeroship_runtime::state::OpResult;

use crate::query;
use crate::v8_bridge::{fmt_db_err, runtime_state, setup_promise};
use crate::DB_POOL;

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

/// Execute DDL for registerModel. Implements the four-phase orchestrator
/// from proposal A2 (`docs/proposals/zeroship-db.md`):
///
///   1. **Bootstrap**: ensure schema exists and the `__zeroship_migrations`
///      audit table is provisioned (A3).
///   2. **Diff phase**: introspect `pg_catalog` and classify each
///      declared change into additive / compatible / destructive.
///   3. **Validate phase**: for compatible/destructive ops that involve
///      an existence check (NOT NULL on a non-empty table, new UNIQUE),
///      run the validation query and short-circuit `strict` deploys.
///   4. **Apply phase**: run additive + compatible DDL (table + columns
///      transactionally, CREATE INDEX CONCURRENTLY outside any tx). Every
///      operation writes an audit row.
///
/// Destructive changes return a structured `validation_refused` envelope.
/// On a fresh deploy where the table doesn't exist, the diff collapses
/// to a single `create_table` op so the cold-start path is still
/// IF NOT EXISTS-idempotent.
///
/// Concurrent-deploy serialisation uses Postgres' two-key advisory lock
/// (`pg_advisory_xact_lock(hashtext('zs_reg:<app_id>')::int4,
/// hashtext(<deploy_id>)::int4)`); a second worker cold-starting against
/// the same app + deploy_id blocks until the first transaction commits
/// (proposal A2, "Concurrent-deploy semantics" section).
async fn exec_register_model(
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
) -> Result<(), String> {
    // Lazy pool init
    let has_pool = DB_POOL.with(|p| p.borrow().is_some());
    if !has_pool {
        crate::init_pool_async()
            .await
            .map_err(|e| format!("db: lazy init failed: {e}"))?;
    }

    let pool = DB_POOL.with(|p| {
        let borrow = p.borrow();
        borrow.as_ref().map(Rc::clone)
    });
    let pool = pool.ok_or_else(|| "db: pool not initialized".to_string())?;

    let deploy_id =
        std::env::var("ZEROSHIP_DEPLOY_ID").unwrap_or_else(|_| "cold_start".to_string());

    exec_register_model_with_pool(&pool, app_id, collection, schema, indexes, &deploy_id).await
}

/// Pool-driven variant of `exec_register_model`. Public so integration
/// tests can drive the four-phase orchestrator without going through V8.
///
/// `deploy_id` controls audit-log grouping (proposal A3 line 233 reserves
/// `'cold_start'` for pre-deploy DDL).
pub async fn exec_register_model_with_pool(
    pool: &compio_postgres::Pool,
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
    deploy_id: &str,
) -> Result<(), String> {
    // Strictness — proposal A2 line 122. Read from schema._meta.strictness
    // if present; default is 'strict'.
    let strictness = schema
        .get("_meta")
        .and_then(|m| m.get("strictness"))
        .and_then(Value::as_str)
        .unwrap_or("strict")
        .to_string();

    let empty: Vec<&str> = Vec::new();

    // -------------------------------------------------------------------
    // Concurrent-deploy serialisation: proposal A2 line 202.
    //
    // Two-key advisory lock keyed on (app_id, register_model). Held at
    // session scope on a dedicated pool client so the lock survives the
    // CREATE INDEX CONCURRENTLY phases (which can't run in a transaction).
    // Released when this function returns (either by explicit unlock or
    // by `lock_client` being dropped — its backend session ends, which
    // implicitly releases all session-level advisory locks).
    //
    // The proposal calls for `pg_advisory_xact_lock` (transaction scope);
    // because registerModel spans non-transactional CONCURRENTLY DDL, we
    // use the session-scoped equivalent `pg_advisory_lock` on a dedicated
    // connection. Functionally identical for our serialisation goal: a
    // second worker calling the same function blocks on the same key.
    let lock_client = pool
        .get()
        .await
        .map_err(|e| format!("db: failed to acquire orchestrator client: {e}"))?;
    let lock_sql =
        "SELECT pg_advisory_lock(hashtext('zs_reg:' || $1)::int4, hashtext('register_model')::int4)";
    lock_client
        .query_text_params(lock_sql, &[app_id])
        .await
        .map_err(|e| format!("db: pg_advisory_lock failed: {e}"))?;
    // From this point on, until lock_client is dropped at function exit,
    // any other orchestrator call against the same app_id blocks.

    // -------------------------------------------------------------------
    // Bootstrap: schema + audit table
    // -------------------------------------------------------------------
    let create_schema = query::build_create_schema(app_id);
    pool.query_text_params(&create_schema, &empty)
        .await
        .map_err(|e| format!("db: create schema failed: {e}"))?;

    crate::audit::ensure_audit_table_exists(pool, app_id).await?;

    let schema_version = crate::audit::next_schema_version(pool, app_id).await?;

    let mut declared_indexes = query::build_create_indexes(app_id, collection, schema)
        .map_err(|e| format!("db: {e}"))?;
    let named_indexes = query::build_named_indexes(app_id, collection, indexes)
        .map_err(|e| format!("db: {e}"))?;
    declared_indexes.extend(named_indexes);

    // -------------------------------------------------------------------
    // Diff phase: introspect pg_catalog, classify changes.
    // -------------------------------------------------------------------
    let mut live = crate::diff::read_live_schema(pool, app_id).await?;
    let rows_estimate = crate::diff::estimate_row_count(pool, app_id, collection).await?;
    live.row_counts.insert(collection.to_string(), rows_estimate);

    // B2 — build CREATE TABLE with Deferred FK emission keyed on the live
    // table set. Refs to tables that already exist inline their FK; refs
    // to tables that don't exist yet skip the inline clause, and the diff
    // engine emits a follow-on `ALTER TABLE … ADD CONSTRAINT` op. This
    // breaks the cross-table cold-start race: concurrent
    // `registerModel("users")` / `registerModel("todos")` calls serialize
    // on the advisory lock; whichever runs second sees the first table in
    // `live` and can inline the FK, or defers it to its own apply phase.
    let existing_tables: std::collections::HashSet<String> =
        live.tables.keys().cloned().collect();
    let create_table = query::build_create_table_with_fks(
        app_id,
        collection,
        schema,
        &query::FkEmission::Deferred(&existing_tables),
    )
    .map_err(|e| format!("db: {e}"))?;

    let ops = crate::diff::compute_diff(
        &live,
        app_id,
        collection,
        schema,
        &create_table,
        &declared_indexes,
    );

    // -------------------------------------------------------------------
    // Classify phase: surface destructive ops as validation_refused
    // when strictness != 'off'. Lenient logs but proceeds with additive
    // + compatible only.
    // -------------------------------------------------------------------
    let destructive: Vec<&crate::diff::DiffOp> = ops
        .iter()
        .filter(|op| op.class == crate::diff::ChangeClass::Destructive)
        .collect();

    if !destructive.is_empty() && strictness != "off" {
        // Audit each destructive op as pending so operators can see what
        // was refused. Then return a validation_refused envelope.
        for op in &destructive {
            let row = crate::audit::AuditRow {
                collection: op.collection.clone(),
                phase: crate::audit::Phase::Ddl,
                change_class: op.class.as_audit(),
                change_kind: op.change_kind.as_sql().to_string(),
                details: op.details.clone(),
                ddl_sql: op.sql.clone(),
                status: crate::audit::InitialStatus::Pending,
                deploy_id: deploy_id.to_string(),
                schema_version,
                actor: crate::audit::ActorKind::Auto,
            };
            // Best-effort: a failure to write the audit row should not
            // mask the envelope — tracing::warn so it shows in worker
            // logs but the user-facing error stays clean.
            if let Err(e) = crate::audit::write_audit_row(pool, app_id, &row).await {
                tracing::warn!(error = %e, "audit: failed to log destructive op");
            }
        }

        if strictness == "strict" {
            return Err(build_validation_refused_envelope(deploy_id, &destructive));
        }
        // strictness == "lenient": fall through, but skip destructive ops.
    }

    // -------------------------------------------------------------------
    // Validate phase: for compatible ops with an existence check
    // (currently: add NOT NULL column with default on a non-empty table —
    // covered by the classifier already; new UNIQUE constraint — handled
    // in create_index_with_recovery via 23505).
    //
    // The exhaustive validation budget loop (proposal A2 line 153) is
    // deferred to a follow-up PR: for the additive-only flows the diff
    // engine now identifies, classification already prevents unsafe DDL.
    // -------------------------------------------------------------------

    // -------------------------------------------------------------------
    // Apply phase: run additive + compatible ops in declared order.
    //
    // Split into two passes:
    //   1. Transactional ops (CREATE TABLE / ADD COLUMN / ADD/DROP FK) run
    //      while the advisory lock is held — they serialise per-app.
    //   2. CREATE INDEX CONCURRENTLY ops run AFTER releasing the advisory
    //      lock. CIC takes an internal snapshot and waits for all other
    //      open snapshots on the target table to finish; another
    //      orchestrator blocked on `pg_advisory_lock` holds a snapshot
    //      that CIC waits on → deadlock. CIC is idempotent via
    //      `IF NOT EXISTS` so it's safe to run unlocked.
    // -------------------------------------------------------------------
    let run_op = async |op: &crate::diff::DiffOp| -> Result<(), String> {
        let audit_id = match crate::audit::write_audit_row(
            pool,
            app_id,
            &crate::audit::AuditRow {
                collection: op.collection.clone(),
                phase: crate::audit::Phase::Ddl,
                change_class: op.class.as_audit(),
                change_kind: op.change_kind.as_sql().to_string(),
                details: op.details.clone(),
                ddl_sql: op.sql.clone(),
                status: crate::audit::InitialStatus::Running,
                deploy_id: deploy_id.to_string(),
                schema_version,
                actor: crate::audit::ActorKind::Auto,
            },
        )
        .await
        {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(error = %e, "audit: failed to insert running row");
                None
            }
        };

        let result = match &op.change_kind {
            crate::diff::ChangeKind::CreateTable
            | crate::diff::ChangeKind::AddColumn
            | crate::diff::ChangeKind::AddForeignKey
            | crate::diff::ChangeKind::DropForeignKey => {
                if let Some(sql) = &op.sql {
                    pool.query_text_params(sql, &empty)
                        .await
                        .map(|_| ())
                        .map_err(|e| {
                            format!("db: {} failed: {}", op.change_kind.as_sql(), fmt_db_err(&e))
                        })
                } else {
                    Ok(())
                }
            }
            crate::diff::ChangeKind::AddIndex => {
                let spec_owned = declared_indexes
                    .iter()
                    .find(|s| {
                        op.details.get("index_name").and_then(Value::as_str)
                            == Some(s.name.as_str())
                    })
                    .cloned();
                if let Some(spec) = spec_owned {
                    create_index_with_recovery_audited(
                        pool,
                        app_id,
                        collection,
                        &spec,
                        deploy_id,
                        schema_version,
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            crate::diff::ChangeKind::DropColumn | crate::diff::ChangeKind::DropIndex => Ok(()),
        };

        if let Some(id) = audit_id {
            match &result {
                Ok(_) => {
                    let _ = crate::audit::update_audit_status(
                        pool,
                        app_id,
                        id,
                        crate::audit::TerminalStatus::Applied,
                        None,
                    )
                    .await;
                }
                Err(e) => {
                    let _ = crate::audit::update_audit_status(
                        pool,
                        app_id,
                        id,
                        crate::audit::TerminalStatus::Failed,
                        Some(e.as_str()),
                    )
                    .await;
                }
            }
        }

        result
    };

    // Pass 1: transactional ops under advisory lock.
    for op in &ops {
        if op.class == crate::diff::ChangeClass::Destructive {
            continue;
        }
        if matches!(op.change_kind, crate::diff::ChangeKind::AddIndex) {
            continue;
        }
        run_op(op).await?;
    }

    // Release advisory lock BEFORE CIC. Two orchestrators racing on CIC
    // is safe (IF NOT EXISTS), but holding the lock through CIC
    // deadlocks: a second waiter blocked on pg_advisory_lock pins a
    // snapshot that CIC waits on.
    let unlock_sql =
        "SELECT pg_advisory_unlock(hashtext('zs_reg:' || $1)::int4, hashtext('register_model')::int4)";
    let _ = lock_client.query_text_params(unlock_sql, &[app_id]).await;
    drop(lock_client);

    // Pass 2: CIC ops, unlocked.
    for op in &ops {
        if op.class == crate::diff::ChangeClass::Destructive {
            continue;
        }
        if !matches!(op.change_kind, crate::diff::ChangeKind::AddIndex) {
            continue;
        }
        run_op(op).await?;
    }

    Ok(())
}

/// Build the `validation_refused` error envelope (proposal A2 line 167).
/// The shape matches the SDK's expected error contract so the deploy
/// pipeline can render the failing PKs / approval URL uniformly.
fn build_validation_refused_envelope(
    deploy_id: &str,
    destructive: &[&crate::diff::DiffOp],
) -> String {
    let pending: Vec<Value> = destructive
        .iter()
        .map(|op| {
            serde_json::json!({
                "collection": op.collection,
                "change_kind": op.change_kind.as_sql(),
                "field": op.field,
                "details": op.details,
            })
        })
        .collect();

    serde_json::json!({
        "code": "validation_refused",
        "deploy_id": deploy_id,
        "violations": [],
        "destructive_pending": pending,
    })
    .to_string()
}

/// Audited variant of `create_index_with_recovery` — every retry,
/// INVALID-detection drop, and terminal failure writes an
/// `index_retry` row to `__zeroship_migrations` so operators can see
/// what the cold-start orchestrator did (proposal A3). Retains the same
/// SQLSTATE policy as the un-audited version.
async fn create_index_with_recovery_audited(
    pool: &compio_postgres::Pool,
    app_id: &str,
    collection: &str,
    spec: &query::IndexSpec,
    deploy_id: &str,
    schema_version: i32,
) -> Result<(), String> {
    use compio_postgres::error::SqlState;

    const MAX_RETRIES: u32 = 3;
    let empty: Vec<&str> = Vec::new();
    let qualified_idx = format!("\"{}\".\"{}\"", app_id, spec.name);
    let drop_idx_sql = format!(
        "DROP INDEX CONCURRENTLY IF EXISTS \"{}\".\"{}\"",
        app_id, spec.name
    );

    let log_retry = |reason: &'static str,
                     attempt: u32,
                     sqlstate: Option<String>,
                     error: Option<String>| {
        crate::audit::AuditRow {
            collection: collection.to_string(),
            phase: crate::audit::Phase::Ddl,
            change_class: if spec.unique {
                crate::audit::ChangeClass::Compatible
            } else {
                crate::audit::ChangeClass::Additive
            },
            change_kind: "index_retry".to_string(),
            details: serde_json::json!({
                "reason": reason,
                "attempt": attempt,
                "index_name": spec.name,
                "columns": spec.columns,
                "unique": spec.unique,
                "sqlstate": sqlstate,
                "error": error,
            }),
            ddl_sql: Some(spec.sql.clone()),
            status: crate::audit::InitialStatus::Running,
            deploy_id: deploy_id.to_string(),
            schema_version,
            actor: crate::audit::ActorKind::Auto,
        }
    };

    for attempt in 0..=MAX_RETRIES {
        let create_res = pool.query_text_params(&spec.sql, &empty).await;

        match create_res {
            Ok(_) => {
                let check_sql = format!(
                    "SELECT indisvalid FROM pg_index WHERE indexrelid = '{}'::regclass",
                    qualified_idx.replace('\'', "''")
                );
                let rows = pool
                    .query_text_params(&check_sql, &empty)
                    .await
                    .map_err(|e| {
                        format!(
                            "db: failed to verify index '{}' validity: {}",
                            spec.name,
                            fmt_db_err(&e)
                        )
                    })?;
                let valid = rows
                    .first()
                    .map(|r| r.try_get::<_, bool>("indisvalid").unwrap_or(false))
                    .unwrap_or(false);
                if valid {
                    return Ok(());
                }

                // INVALID index — audit the retry, drop, and loop.
                let row = log_retry("invalid_index_landed", attempt, None, None);
                if let Ok(id) = crate::audit::write_audit_row(pool, app_id, &row).await {
                    let _ = crate::audit::update_audit_status(
                        pool,
                        app_id,
                        id,
                        crate::audit::TerminalStatus::Failed,
                        Some("index landed INVALID"),
                    )
                    .await;
                }
                let _ = pool.query_text_params(&drop_idx_sql, &empty).await;
                if attempt == MAX_RETRIES {
                    return Err(format!(
                        "{{\"code\":\"validation_refused\",\"change_kind\":\"index_retry\",\
                        \"collection\":\"{}\",\"index\":\"{}\",\
                        \"reason\":\"index repeatedly landed INVALID after {} retries\"}}",
                        collection, spec.name, MAX_RETRIES
                    ));
                }
            }
            Err(e) => {
                let code = e.code().cloned();
                let fatal = matches!(
                    code.as_ref(),
                    Some(c) if c == &SqlState::UNIQUE_VIOLATION
                        || c == &SqlState::NOT_NULL_VIOLATION
                        || c == &SqlState::FOREIGN_KEY_VIOLATION
                        || c == &SqlState::CHECK_VIOLATION
                );
                if fatal {
                    let code_str = code.as_ref().map(|c| c.code()).unwrap_or("23xxx");
                    let constraint_kind = if spec.unique { "unique" } else { "index" };
                    let row = log_retry(
                        "data_violation",
                        attempt,
                        Some(code_str.to_string()),
                        Some(fmt_db_err(&e)),
                    );
                    if let Ok(id) = crate::audit::write_audit_row(pool, app_id, &row).await {
                        let _ = crate::audit::update_audit_status(
                            pool,
                            app_id,
                            id,
                            crate::audit::TerminalStatus::Failed,
                            Some("data violates constraint"),
                        )
                        .await;
                    }
                    let _ = pool.query_text_params(&drop_idx_sql, &empty).await;
                    return Err(format!(
                        "{{\"code\":\"unique_violation\",\"sqlstate\":\"{}\",\
                        \"collection\":\"{}\",\"constraint\":\"{}\",\
                        \"index\":\"{}\",\"columns\":{:?},\
                        \"message\":\"{}\"}}",
                        code_str,
                        collection,
                        constraint_kind,
                        spec.name,
                        spec.columns,
                        fmt_db_err(&e).replace('"', "\\\"")
                    ));
                }

                let transient = matches!(
                    code.as_ref(),
                    Some(c) if c == &SqlState::T_R_DEADLOCK_DETECTED
                        || c == &SqlState::DISK_FULL
                        || c == &SqlState::OUT_OF_MEMORY
                );

                let row = log_retry(
                    if transient {
                        "transient_retry"
                    } else {
                        "non_transient_failure"
                    },
                    attempt,
                    code.as_ref().map(|c| c.code().to_string()),
                    Some(fmt_db_err(&e)),
                );
                if let Ok(id) = crate::audit::write_audit_row(pool, app_id, &row).await {
                    let _ = crate::audit::update_audit_status(
                        pool,
                        app_id,
                        id,
                        crate::audit::TerminalStatus::Failed,
                        Some("index build failed"),
                    )
                    .await;
                }

                let _ = pool.query_text_params(&drop_idx_sql, &empty).await;
                if !transient || attempt == MAX_RETRIES {
                    return Err(format!(
                        "{{\"code\":\"validation_refused\",\"change_kind\":\"index_retry\",\
                        \"collection\":\"{}\",\"index\":\"{}\",\
                        \"sqlstate\":\"{}\",\"attempts\":{},\
                        \"message\":\"{}\"}}",
                        collection,
                        spec.name,
                        code.as_ref().map(|c| c.code()).unwrap_or("unknown"),
                        attempt + 1,
                        fmt_db_err(&e).replace('"', "\\\"")
                    ));
                }
            }
        }
    }

    Err(format!(
        "db: create index '{}' exhausted retry budget without a terminal result",
        spec.name
    ))
}
