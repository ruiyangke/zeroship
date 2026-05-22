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
use crate::error::DbError;

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
) -> Result<(), DbError> {
    let RegisterContext {
        app_id,
        deploy_id,
        schema_version,
        strictness: _,
        declared_indexes,
    } = ctx;

    let run_op = async |op: &DiffOp| -> Result<(), DbError> {
        // Contract gate — see `check_destructive_invariant`.
        //
        // The pass1/pass2 loops filter `ChangeClass::Destructive` out
        // BEFORE calling `run_op`, so any `DropColumn`/`DropIndex` that
        // reaches here has lost (or never had) its destructive tag.
        // Surface that as `DbError::Internal` instead of silently
        // succeeding — the previous catch-all `Ok(())` would write a
        // bogus `Applied` audit row for a no-op. We perform the check
        // BEFORE the `write_audit_row` call so no orphan `Running`
        // row is emitted for a contract violation.
        check_destructive_invariant(op)?;

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
                tracing::warn!(error = ?e, "audit: failed to insert running row");
                None
            }
        };

        let result: Result<(), DbError> = match &op.change_kind {
            ChangeKind::CreateTable
            | ChangeKind::AddColumn
            | ChangeKind::AddForeignKey
            | ChangeKind::DropForeignKey => {
                if let Some(sql) = &op.sql {
                    backend
                        .pool_exec(sql, &[])
                        .await
                        .map(|_| ())
                        .map_err(|e| match e {
                            // Preserve the operator-facing prefix
                            // ("db: ADD COLUMN failed: ...") only for
                            // the catch-all Internal arm; SQLSTATE-coded
                            // variants (unique_violation, fk_violation,
                            // lock_not_available, …) reach JS verbatim
                            // so the SDK can branch on `.code`.
                            DbError::Internal { message } => DbError::Internal {
                                message: format!(
                                    "db: {} failed: {}",
                                    op.change_kind.as_sql(),
                                    message
                                ),
                            },
                            other => other,
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
                    // `create_index_with_recovery` returns a typed
                    // `DbError`. `SchemaRefused` carries the JSON
                    // envelope the SDK already consumes via
                    // `JSON.parse`, `UniqueViolation`/`Transient`/…
                    // flow through the standard SQLSTATE classification,
                    // and `Configuration` surfaces invariant breaches.
                    // No wrapping or string-rail bridging required.
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
            // Unreachable in practice — `check_destructive_invariant`
            // above turns any `DropColumn`/`DropIndex` that survives
            // the upstream destructive-class filter into an error
            // BEFORE we reach this match. The arm is kept (returning
            // the same error) so the compiler enforces exhaustive
            // matching: a future variant added to `ChangeKind` will
            // fail to compile here, forcing an explicit decision.
            ChangeKind::DropColumn | ChangeKind::DropIndex => {
                Err(destructive_invariant_error(op))
            }
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
                    // Render the typed error to a flat string for the
                    // audit_error column. The DbError variants `to_op_error()`
                    // strips for JS are recoverable here only as the
                    // message body.
                    let msg = e.clone().into_string();
                    let _ = backend
                        .update_audit_status(
                            &app_id,
                            id,
                            crate::audit::TerminalStatus::Failed,
                            Some(msg.as_str()),
                        )
                        .await;
                }
            }
        }

        result
    };

    // Pass 1: transactional ops under advisory lock.
    //
    // Run the loop inside an async block so we can capture its Result
    // and ALWAYS release the advisory lock — even on early `?`
    // propagation. Without this, an error path returns directly to the
    // caller and `PooledClient::Drop` parks the connection back in the
    // pool with its session-scoped lock still held, blocking every
    // subsequent caller (cross-app stall).
    let pass1: Result<(), DbError> = async {
        for op in &approved.ops {
            if op.class == ChangeClass::Destructive {
                continue;
            }
            if matches!(op.change_kind, ChangeKind::AddIndex) {
                continue;
            }
            run_op(op).await?;
        }
        Ok(())
    }
    .await;

    // Release advisory lock BEFORE CIC, **regardless of Pass 1 outcome**.
    // Two orchestrators racing on CIC is safe (IF NOT EXISTS), but
    // holding the lock through CIC deadlocks: a second waiter blocked on
    // pg_advisory_lock pins a snapshot that CIC waits on. And if Pass 1
    // errored, we MUST still unlock — otherwise the pooled connection
    // returns to the pool with the session-scoped lock held.
    //
    // Issue `pg_advisory_unlock` explicitly so the lock count
    // decrements while the connection is still parked.
    let unlock_sql =
        "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
    let key = lock_key(&app_id);
    let _ = lock_client
        .query_text_params(unlock_sql, &[key.as_str(), LOCK_TAG])
        .await;
    drop(lock_client);

    // Propagate Pass 1 error after the lock has been released.
    pass1?;

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

/// Enforce the contract that any `DropColumn` / `DropIndex` op reaching
/// the apply layer must carry `ChangeClass::Destructive` — the upstream
/// pass1 / pass2 loops filter destructive ops out before they ever
/// reach `run_op`, so a `Drop*` op that gets here has slipped past
/// that filter (or never had its class set correctly by the diff
/// engine).
///
/// Silently returning `Ok(())` for this case — as the pre-fix code
/// did — would write a bogus `Applied` audit row, masking a real bug
/// in either the diff classifier or the destructive-skip loop. The
/// fix surfaces an `Internal` error naming the breached contract so
/// the operator sees the issue at the next deploy instead of
/// discovering a silently-skipped drop weeks later.
///
/// Non-`Drop*` change kinds pass through with `Ok(())` — they have
/// their own SQL paths in the `match` block above and don't share
/// this contract.
fn check_destructive_invariant(op: &DiffOp) -> Result<(), DbError> {
    if matches!(
        op.change_kind,
        ChangeKind::DropColumn | ChangeKind::DropIndex
    ) && op.class != ChangeClass::Destructive
    {
        return Err(destructive_invariant_error(op));
    }
    Ok(())
}

/// Build the diagnostic message for a destructive-invariant violation.
/// Named so the unreachable `DropColumn | DropIndex` arm inside the
/// `match` block can produce an identical error without duplicating
/// the message string.
fn destructive_invariant_error(op: &DiffOp) -> DbError {
    let msg = format!(
        "db: apply received a {} op outside ChangeClass::Destructive \
         (class={:?}); upstream destructive-class filter \
         (register_model::apply pass1/pass2) should have skipped it. \
         This is a contract violation — refusing to silently no-op.",
        op.change_kind.as_sql(),
        op.class,
    );
    tracing::error!(
        change_kind = op.change_kind.as_sql(),
        class = ?op.class,
        collection = %op.collection,
        "apply: destructive-invariant violation"
    );
    DbError::Internal { message: msg }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the destructive-invariant contract gate.
    //!
    //! These exercise `check_destructive_invariant` directly rather than
    //! the full `apply()` function — `apply` takes a
    //! `compio_postgres::PooledClient<'p>` and issues a
    //! `pg_advisory_unlock` against it, so end-to-end coverage requires
    //! a live Postgres listener and lives in
    //! `crates/plugin-db/tests/integration.rs`. The gate itself is a
    //! pure predicate over `DiffOp`, which is what these tests pin.
    use super::*;
    use crate::diff::{ChangeClass, ChangeKind, DiffOp};

    fn diff_op(kind: ChangeKind, class: ChangeClass) -> DiffOp {
        DiffOp {
            collection: "posts".into(),
            change_kind: kind,
            class,
            sql: None,
            details: serde_json::json!({}),
            field: Some("legacy_col".into()),
        }
    }

    /// A `DropColumn` op reaching apply without `ChangeClass::Destructive`
    /// means the destructive-class filter above failed (or the diff
    /// engine emitted a misclassified op). The gate MUST return
    /// `DbError::Internal` so the failure is visible — silent `Ok(())`
    /// would write a fake `Applied` audit row.
    #[test]
    fn drop_column_without_destructive_class_returns_internal_error() {
        let op = diff_op(ChangeKind::DropColumn, ChangeClass::Additive);
        let result = check_destructive_invariant(&op);
        match result {
            Err(DbError::Internal { message }) => {
                // The message must name the contract that was breached
                // so the operator can locate the upstream regression.
                assert!(
                    message.contains("drop_column"),
                    "message should name the change_kind: {message}"
                );
                assert!(
                    message.contains("Destructive"),
                    "message should name the breached invariant: {message}"
                );
                assert!(
                    message.contains("contract violation"),
                    "message should mark this as a contract violation: {message}"
                );
            }
            other => panic!("expected DbError::Internal, got {other:?}"),
        }
    }

    /// A `DropColumn` op tagged `ChangeClass::Destructive` is the
    /// canonical path — the pass1/pass2 loops in `apply()` skip it
    /// BEFORE `run_op` is invoked, so the gate never sees it during
    /// real applies. We assert the gate is permissive here so a future
    /// refactor that routes destructive ops THROUGH the gate (e.g.
    /// for an audited "refused" trail) doesn't get mis-flagged. No
    /// audit row, no error.
    #[test]
    fn drop_column_with_destructive_class_is_skipped_cleanly() {
        let op = diff_op(ChangeKind::DropColumn, ChangeClass::Destructive);
        assert!(
            check_destructive_invariant(&op).is_ok(),
            "destructive-class drops must pass the gate (the upstream filter \
             is the canonical skip; the gate only fires on misclassified ops)"
        );

        // Same for DropIndex — the gate is shape-symmetric.
        let op = diff_op(ChangeKind::DropIndex, ChangeClass::Destructive);
        assert!(check_destructive_invariant(&op).is_ok());
    }

    /// Non-`Drop*` change kinds are out of scope for this invariant —
    /// they have their own SQL paths in `run_op`'s match block. The
    /// gate must not interfere.
    #[test]
    fn non_drop_change_kinds_pass_the_gate_regardless_of_class() {
        for kind in [
            ChangeKind::CreateTable,
            ChangeKind::AddColumn,
            ChangeKind::AddIndex,
            ChangeKind::AddForeignKey,
            ChangeKind::DropForeignKey,
        ] {
            for class in [
                ChangeClass::Additive,
                ChangeClass::Compatible,
                ChangeClass::Destructive,
            ] {
                let op = diff_op(kind.clone(), class);
                assert!(
                    check_destructive_invariant(&op).is_ok(),
                    "gate must not fire on {kind:?} / {class:?}",
                );
            }
        }
    }
}
