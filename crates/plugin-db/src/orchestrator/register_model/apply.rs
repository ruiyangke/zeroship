//! Stage 4 — Apply.
//!
//! Executes the [`ApprovedPlan`] in two passes:
//!
//! 1. **Pass 1** — transactional ops (CREATE TABLE / ADD COLUMN /
//!    ADD/DROP FK) run while the advisory lock from
//!    [`bootstrap`](super::bootstrap) is still held. These serialise
//!    per-app.
//! 2. **Pass 2** — `CREATE INDEX CONCURRENTLY` ops run AFTER the
//!    advisory lock is released (the dedicated `lock_client` is dropped
//!    between passes, ending its backend session and releasing the
//!    session-scoped lock). Holding the lock through CIC would
//!    deadlock: a second waiter blocked on `pg_advisory_lock` pins a
//!    snapshot that CIC waits on. CIC is idempotent via `IF NOT EXISTS`
//!    so it's safe to run unlocked.
//!
//! Every op writes a `running` row to `__zeroship_migrations` before
//! execution and a `applied` / `failed` terminal row after. The audited
//! `create_index_with_recovery_audited` adds extra rows for retry /
//! invalid-index / data-violation paths.

use compio_postgres::Pool;
use serde_json::Value;

use super::bootstrap::RegisterContext;
use super::validate::ApprovedPlan;
use crate::diff::{ChangeClass, ChangeKind, DiffOp};
use crate::query;
use crate::v8_bridge::fmt_db_err;

/// Run stage 4.
///
/// Consumes the [`RegisterContext`] because pass 2 must drop
/// `lock_client` to release the advisory lock between passes.
pub(crate) async fn apply(
    pool: &Pool,
    ctx: RegisterContext<'_>,
    approved: ApprovedPlan,
) -> Result<(), String> {
    let RegisterContext {
        app_id,
        deploy_id,
        schema_version,
        strictness: _,
        declared_indexes,
        lock_client,
    } = ctx;

    let empty: Vec<&str> = Vec::new();

    let run_op = async |op: &DiffOp| -> Result<(), String> {
        let audit_id = match crate::audit::write_audit_row(
            pool,
            &app_id,
            &crate::audit::AuditRow {
                collection: op.collection.clone(),
                phase: crate::audit::Phase::Ddl,
                change_class: op.class.as_audit(),
                change_kind: op.change_kind.as_sql().to_string(),
                details: op.details.clone(),
                ddl_sql: op.sql.clone(),
                status: crate::audit::InitialStatus::Running,
                deploy_id: deploy_id.clone(),
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
            ChangeKind::CreateTable
            | ChangeKind::AddColumn
            | ChangeKind::AddForeignKey
            | ChangeKind::DropForeignKey => {
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
            ChangeKind::AddIndex => {
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
                        &app_id,
                        &op.collection,
                        &spec,
                        &deploy_id,
                        schema_version,
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            ChangeKind::DropColumn | ChangeKind::DropIndex => Ok(()),
        };

        if let Some(id) = audit_id {
            match &result {
                Ok(_) => {
                    let _ = crate::audit::update_audit_status(
                        pool,
                        &app_id,
                        id,
                        crate::audit::TerminalStatus::Applied,
                        None,
                    )
                    .await;
                }
                Err(e) => {
                    let _ = crate::audit::update_audit_status(
                        pool,
                        &app_id,
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
    for op in &approved.ops {
        if op.class == ChangeClass::Destructive {
            continue;
        }
        if matches!(op.change_kind, ChangeKind::AddIndex) {
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
    let _ = lock_client.query_text_params(unlock_sql, &[app_id.as_str()]).await;
    drop(lock_client);

    // Pass 2: CIC ops, unlocked.
    for op in &approved.ops {
        if op.class == ChangeClass::Destructive {
            continue;
        }
        if !matches!(op.change_kind, ChangeKind::AddIndex) {
            continue;
        }
        run_op(op).await?;
    }

    Ok(())
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
