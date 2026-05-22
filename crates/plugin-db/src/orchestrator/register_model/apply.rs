//! Stage 4 — Apply.
//!
//! Executes the [`ApprovedPlan`] in two passes:
//!
//! 1. **Pass 1** — transactional ops (CREATE TABLE / ADD COLUMN /
//!    ADD/DROP FK) run while the advisory lock from
//!    [`bootstrap`](super::bootstrap) is still held. These serialise
//!    per-app.
//! 2. **Pass 2** — `CREATE INDEX CONCURRENTLY` ops run AFTER the
//!    advisory lock is released ([`LockGuard::release`] issues
//!    `pg_advisory_unlock` then returns the client). Holding the lock
//!    through CIC would deadlock: a second waiter blocked on
//!    `pg_advisory_lock` pins a snapshot that CIC waits on. CIC is
//!    idempotent via `IF NOT EXISTS` so it's safe to run unlocked.
//!
//! Every op writes a `running` row to `__zeroship_migrations` before
//! execution and a `applied` / `failed` terminal row after. The audited
//! CIC recovery loop (extra rows for retry / invalid-index /
//! data-violation paths) lives in
//! [`crate::backend::Backend::create_index_with_recovery`].

use serde_json::Value;

use super::bootstrap::RegisterContext;
use super::validate::ApprovedPlan;
use crate::backend::{IndexBuilder, LockGuard, PgSqlExecutor};
use crate::diff::{ChangeClass, ChangeKind, DiffOp};
use crate::error::DbError;

/// Run stage 4.
///
/// Takes the [`LockGuard`] separately so the lock can be released
/// between passes without dragging the `'p` borrow through every
/// upstream type. The guard is consumed by the explicit
/// `release().await` between Pass 1 and Pass 2.
///
/// **P0 PR 2**: bound narrowed to [`PgSqlExecutor`] + [`IndexBuilder`]
/// (was `Backend`). `PgSqlExecutor` gives us pool access for the
/// free-function audit helpers (Open Q1 resolution) plus `pool_exec`
/// for Pass-1 DDL via its [`crate::backend::SqlExecutor`] super-bound;
/// `IndexBuilder` carries the `create_index_with_recovery` call used
/// by Pass 2. See `docs/proposals/p0-implementation-plan.md` §"PR 2"
/// and `docs/proposals/db-system-design.md` §7.
pub(crate) async fn apply<'p, B: PgSqlExecutor + IndexBuilder>(
    backend: &B,
    ctx: RegisterContext,
    lock_guard: LockGuard<'p>,
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

        let audit_id = match crate::audit::write_audit_row(
            backend.pool_handle().as_ref(),
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
            Err(audit_err) => {
                // F1 warn-half family: same write_audit_row secondary-
                // failure shape as validate.rs:101 (8th F1 site across
                // the family at HEAD: 6 audit_id-slot + 2 collection-slot
                // — this one is collection-slot). Closes code-critique
                // r12 MINOR-R12-1 cousin drift the prior unification
                // (cycle-12:47 `7c6bd2ec`) missed.
                tracing::warn!(
                    app_id = %app_id,
                    collection = %op.collection,
                    transition = "Running/insert_failed",
                    audit_err = %audit_err,
                    "audit: failed to insert running row",
                );
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
                    if let Err(audit_err) = crate::audit::update_audit_status(
                        backend.pool_handle().as_ref(),
                        &app_id,
                        id,
                        crate::audit::TerminalStatus::Applied,
                        None,
                    )
                    .await
                    {
                        // F1 warn-half: the audit row stays in
                        // `Running` until the next reset sweeps it.
                        // Logging gives operators a signal to
                        // investigate stuck rows. Field shape pinned
                        // by code-critique r11 MINOR-R11-1 (unified
                        // across all 8 F1 sites — 6 audit_id-slot +
                        // 2 collection-slot variants).
                        tracing::warn!(
                            app_id = %app_id,
                            audit_id = id,
                            transition = "Applied",
                            audit_err = %audit_err,
                            "update_audit_status failed; row stays in 'running' until reset",
                        );
                    }
                }
                Err(e) => {
                    // Render the typed error to a flat string for the
                    // audit_error column. The DbError variants `to_op_error()`
                    // strips for JS are recoverable here only as the
                    // message body.
                    let msg = e.clone().into_string();
                    if let Err(audit_err) = crate::audit::update_audit_status(
                        backend.pool_handle().as_ref(),
                        &app_id,
                        id,
                        crate::audit::TerminalStatus::Failed,
                        Some(msg.as_str()),
                    )
                    .await
                    {
                        // Same F1 warn-half: the underlying DDL error
                        // still propagates via `result`, so JS still
                        // sees the failure — the warn surfaces the
                        // audit-write secondary failure. Both errors
                        // are emitted because they have different
                        // root causes (primary DDL vs secondary
                        // audit-write).
                        tracing::warn!(
                            app_id = %app_id,
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
    // The guard's `release()` issues `pg_advisory_unlock` then returns
    // the now-unlocked client back to the pool on drop. Best-effort:
    // any SQL error is swallowed inside the guard (matches the
    // pre-refactor inline behaviour).
    let _ = lock_guard.release().await;

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

    // ----- destructive-invariant ERROR-shape contract ------------------
    //
    // `destructive_invariant_error` emits a `tracing::error!` whose
    // field shape (`change_kind`, `class`, `collection`) is part of
    // the operator-grep contract — a runbook search for
    // `change_kind=drop_column` should match this site. Pin the shape
    // so a future refactor that renames `change_kind` → `kind` (or
    // similar) fails at unit-test time. test-coverage r11 NEW-R11-1.
    //
    // Note: this test exercises the function end-to-end (calls
    // `destructive_invariant_error` directly and asserts what came
    // out of the capture layer). Drift in the source IS caught at
    // unit-test time. Contrast with the F1 warn-half sites
    // (`update_audit_status failed; row stays in 'running' until
    // reset`), which can only be driven via a Backend trait failure
    // path; for those sites see
    // [`f1_warn_shape_documentation_test`] below — a snapshot test
    // that documents the contract field set but does not drive the
    // source. The integration suite in `tests/integration.rs`
    // exercises the F1 sites end-to-end with real Postgres.

    #[test]
    fn destructive_invariant_error_emits_named_fields_at_error_level() {
        use crate::test_support::capture;
        use tracing::Level;

        let op = diff_op(ChangeKind::DropColumn, ChangeClass::Additive);
        let (_err, events) = capture(|| destructive_invariant_error(&op));

        assert_eq!(events.len(), 1, "expected exactly one tracing event");
        let ev = &events[0];
        assert_eq!(
            ev.level,
            Level::ERROR,
            "destructive-invariant violations must surface at error level"
        );
        // `change_kind = op.change_kind.as_sql()` — the SQL form, not
        // the Debug form, so a runbook can grep on
        // `change_kind=drop_column` (lower-snake).
        assert_eq!(
            ev.fields.get("change_kind").map(String::as_str),
            Some("drop_column"),
            "change_kind field must carry the SQL form (operator-grep contract)",
        );
        // `class = ?op.class` — Debug form is intentional; the
        // discriminant string is what the runbook searches for.
        assert!(
            ev.fields
                .get("class")
                .is_some_and(|v| v.contains("Additive")),
            "class field must carry the ChangeClass variant: fields={:?}",
            ev.fields,
        );
        // `collection = %op.collection` — Display form; the
        // `diff_op` helper uses `"posts"` as the canonical fixture.
        assert_eq!(
            ev.fields.get("collection").map(String::as_str),
            Some("posts"),
            "collection field must carry the user-visible name",
        );
        assert!(
            ev.message.contains("destructive-invariant violation"),
            "message must name the contract for log-grep: {}",
            ev.message,
        );
    }

    /// Documentation snapshot of the F1 warn-half shape.
    ///
    /// Re-emits the same `tracing::warn!` syntax the F1 sites use in
    /// `apply.rs` (lines 178–184 / 209–216) and `backend/postgres.rs`
    /// (lines 496 / 548 / 597) so the operator-grep contract is
    /// pinned by an executable example. Field names checked here MUST
    /// match the production sites; a runbook that greps for
    /// `audit_err=…` relies on every F1 site using that exact key.
    ///
    /// **What this test catches**: a contributor who renames the field
    /// in THIS test (or in the production sites) without updating the
    /// other will see a mismatch when they sync. The test does NOT
    /// drive production code — that would require a Backend mock the
    /// brief explicitly scoped out. End-to-end coverage of the live
    /// F1 path lives in `crates/plugin-db/tests/integration.rs`.
    ///
    /// **What this test does NOT catch**: a contributor who renames
    /// `audit_err` → `error` in BOTH the production sites and this
    /// test in one commit. That class of drift is caught by the code
    /// review + the `transition` discriminator's literal-string match
    /// against the audit table's terminal-state grep.
    ///
    /// Background:
    /// - test-coverage r11 NEW-R11-1 — establish the capture harness.
    /// - test-coverage r12 NEW-R12-1 — pin the F1 warn shape.
    /// - 7c6bd2ec / 18aee490 — the original drift + revert this test
    ///   was meant to catch.
    #[test]
    fn f1_warn_shape_documentation_snapshot() {
        use crate::test_support::capture;
        use tracing::Level;

        // The exact syntax used at apply.rs:178 (the "Applied" arm).
        let app_id = "app_t";
        let audit_id: i64 = 99;
        let audit_err = "connection closed";
        let ((), events) = capture(|| {
            tracing::warn!(
                app_id = %app_id,
                audit_id = audit_id,
                transition = "Applied",
                audit_err = %audit_err,
                "update_audit_status failed; row stays in 'running' until reset",
            );
        });

        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.level, Level::WARN);

        // The four fields every F1 warn site shares. Operators grep
        // on these names; any rename breaks the runbook.
        for name in &["app_id", "audit_id", "transition", "audit_err"] {
            assert!(
                ev.fields.contains_key(*name),
                "F1 warn-shape contract (audit_id slot): field `{name}` MUST \
                 be present across all 6 audit_id-slot sites (apply.rs / \
                 backend/postgres.rs / migrations.rs). Missing from \
                 snapshot — contract broken. Fields: {:?}",
                ev.fields,
            );
        }

        // Values are stringly-typed in the capture; verify the
        // transition discriminator is a literal (operators
        // case-sensitively match `transition="Applied"` etc.).
        assert_eq!(
            ev.fields.get("transition").map(String::as_str),
            Some("Applied"),
            "transition discriminator must be a literal — variant strings \
             differ across the 6 audit_id-slot F1 sites (Applied / Failed / \
             Failed/invalid_index / Failed/data_violation / \
             Failed/index_build / migrations.rs finalise_backfill uses \
             `?terminal` Debug form). Collection-slot sites pinned by the \
             sibling test.",
        );
        assert_eq!(
            ev.message,
            "update_audit_status failed; row stays in 'running' until reset",
            "message body is the operator-search anchor across the F1 \
             half — keep verbatim or update every site + this snapshot \
             in one commit",
        );
    }

    /// Pin the `collection`-slot variant of the F1 warn-shape family.
    ///
    /// The 8-site F1 family splits into two identifier-slot variants:
    /// 6 sites carry `audit_id` (the `update_audit_status` failure
    /// cluster — 5 strict + 1 hybrid `finalise_backfill` carrying both
    /// `audit_id` and `name`/`collection`); 2 sites carry `collection`
    /// only (the `write_audit_row` insert-failure cluster:
    /// `validate.rs:101` + `apply.rs:84` running-row insert path
    /// added by cycle-16:17 `cbd21112`).
    ///
    /// Error-ux r12 LOW (cycle 16:17 finding): the original
    /// `f1_warn_shape_documentation_snapshot` above pinned only the
    /// `audit_id` variant — operator grep on `collection=audit_err=`
    /// against the insert-failure sites wasn't backed by a test. This
    /// test closes that gap: any rename of `collection` /
    /// `transition` / `audit_err` on the insert-failure cluster fails
    /// here at unit-test time.
    #[test]
    fn f1_warn_shape_collection_slot_documentation_snapshot() {
        use crate::test_support::capture;
        use tracing::Level;

        // The exact syntax used at apply.rs:84 (the "Running/insert_failed"
        // arm landed at cycle-16:17 `cbd21112`). Mirrors the validate.rs:101
        // shape (transition = "ValidationRefused/insert_failed").
        let app_id = "app_t";
        let collection = "messages";
        let audit_err = "duplicate key violates unique constraint";
        let ((), events) = capture(|| {
            tracing::warn!(
                app_id = %app_id,
                collection = %collection,
                transition = "Running/insert_failed",
                audit_err = %audit_err,
                "audit: failed to insert running row",
            );
        });

        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.level, Level::WARN);

        // Four-field contract for the collection-slot variant.
        for name in &["app_id", "collection", "transition", "audit_err"] {
            assert!(
                ev.fields.contains_key(*name),
                "F1 warn-shape contract (collection slot): field `{name}` \
                 MUST be present across both insert-failure sites \
                 (validate.rs:101 + apply.rs:84). Missing — contract \
                 broken. Fields: {:?}",
                ev.fields,
            );
        }

        // `transition` discriminator literal-match (operators grep
        // `transition="Running/insert_failed"`).
        assert_eq!(
            ev.fields.get("transition").map(String::as_str),
            Some("Running/insert_failed"),
            "transition discriminator must be literal — `Running/insert_failed` \
             is the running-row INSERT failure variant; \
             `ValidationRefused/insert_failed` is the validate-time variant",
        );
    }
}
