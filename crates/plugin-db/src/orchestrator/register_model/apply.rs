//! Stage 4 — Apply.
//!
//! Executes the [`ApprovedPlan`] in two passes:
//!
//! 1. **Pass 1** — transactional ops (CREATE TABLE / ADD COLUMN /
//!    ADD/DROP FK) run while the advisory lock from
//!    [`bootstrap`](super::bootstrap) is still held. These serialise
//!    per-app.
//! 2. **Pass 2** — `CREATE INDEX CONCURRENTLY` ops run AFTER the
//!    advisory lock is released (the pooled `lock_client` is dropped
//!    between passes, returning the connection to the pool after we
//!    issue an explicit `pg_advisory_unlock`). Holding the lock
//!    through CIC would deadlock: a second waiter blocked on
//!    `pg_advisory_lock` pins a snapshot that CIC waits on. CIC is
//!    idempotent via `IF NOT EXISTS` so it's safe to run unlocked.
//!
//! Every op writes a `running` row to `__zeroship_migrations` before
//! execution and a `applied` / `failed` terminal row after. The audited
//! CIC recovery loop (extra rows for retry / invalid-index /
//! data-violation paths) lives in
//! [`crate::backend::Backend::create_index_with_recovery`].

use compio_postgres::PooledClient;
use serde_json::Value;

use super::bootstrap::{lock_key, RegisterContext, LOCK_TAG};
use super::validate::ApprovedPlan;
use crate::backend::Backend;
use crate::diff::{ChangeClass, ChangeKind, DiffOp};

/// Run stage 4.
///
/// Takes the `lock_client` separately so the lock can be released
/// between passes without dragging the `'p` borrow through every
/// upstream type.
pub(crate) async fn apply<'p, B: Backend>(
    backend: &B,
    ctx: RegisterContext,
    lock_client: PooledClient<'p>,
    approved: ApprovedPlan,
) -> Result<(), String> {
    let RegisterContext {
        app_id,
        deploy_id,
        schema_version,
        strictness: _,
        declared_indexes,
    } = ctx;

    let run_op = async |op: &DiffOp| -> Result<(), String> {
        let audit_id = match backend
            .write_audit_row(
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
                tracing::warn!(error = %e.into_string(), "audit: failed to insert running row");
                None
            }
        };

        let result = match &op.change_kind {
            ChangeKind::CreateTable
            | ChangeKind::AddColumn
            | ChangeKind::AddForeignKey
            | ChangeKind::DropForeignKey => {
                if let Some(sql) = &op.sql {
                    backend
                        .pool_exec(sql, &[])
                        .await
                        .map(|_| ())
                        .map_err(|e| {
                            format!(
                                "db: {} failed: {}",
                                op.change_kind.as_sql(),
                                e.into_string()
                            )
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
                    backend
                        .create_index_with_recovery(
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
                    let _ = backend
                        .update_audit_status(
                            &app_id,
                            id,
                            crate::audit::TerminalStatus::Applied,
                            None,
                        )
                        .await;
                }
                Err(e) => {
                    let _ = backend
                        .update_audit_status(
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
    //
    // Issue `pg_advisory_unlock` explicitly so the lock count
    // decrements while the connection is still parked. The
    // PooledClient then returns to the pool unlocked for the next
    // caller to reuse.
    let unlock_sql =
        "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
    let key = lock_key(&app_id);
    let _ = lock_client
        .query_text_params(unlock_sql, &[key.as_str(), LOCK_TAG])
        .await;
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
