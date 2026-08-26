//! Stage 4 — Apply.
//!
//! Executes the [`ApprovedPlan`] in ONE pass, under the advisory lock from
//! [`bootstrap`](super::bootstrap), releasing the lock on every exit path.
//!
//! # It applies no schema change
//!
//! registerModel does not own schema; a separate migration process does
//! (`crates/zeroship-migrated` at deploy, the vite plugin's dev apply locally).
//! [`super::validate`] refuses every op that changes shape, so exactly one kind
//! reaches this stage: `MaskRewrite`, an UPDATE recomputing a `_masked` sibling
//! whose column already exists. Everything else hits the invariant arm and
//! fails loudly.
//!
//! THIS USED TO BE A TWO-PASS STAGE and the shape is worth recording, because
//! the second pass looked load-bearing right up until it wasn't. Pass 1 ran
//! transactional DDL under the lock; pass 2 ran `CREATE INDEX CONCURRENTLY`
//! after releasing it, because holding the lock through CIC deadlocks - a
//! second waiter blocked on `pg_advisory_lock` pins a snapshot CIC waits on.
//! When the DDL left, pass 2's only selector (`AddIndex`) became something
//! validate refuses, so the pass could never select an op and the release-then-
//! continue dance guarded a hazard that could not arise. Both are deleted.
//!
//! Every op still writes a `running` row to `__zeroship_migrations` before
//! execution and an `applied` / `failed` terminal row after: that table is an
//! operator-facing record, not scaffolding for the DDL that left.

use serde_json::Value;

use super::bootstrap::RegisterContext;
use super::validate::ApprovedPlan;
use crate::backend::{EncryptedColumn, LockGuard, PgSqlExecutor};
use crate::diff::{ChangeKind, DiffOp};
use crate::error::DbError;

/// Run stage 4.
///
/// Takes the [`LockGuard`] separately so the lock can be released without
/// dragging the `'p` borrow through every upstream type. The guard is consumed
/// by the explicit `release().await` after the loop, on success and failure
/// alike.
///
/// `PgSqlExecutor` gives pool access for the free-function audit helpers.
/// `EncryptedColumn` lets the `MaskRewrite` dispatch decrypt an encrypted
/// column before recomputing its mask, via `dispatch_mask_rewrite_op`.
pub(crate) async fn apply<'p, B: PgSqlExecutor + EncryptedColumn>(
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
        declared_indexes: _,
        collection: _ctx_collection,
        schema_json,
    } = ctx;

    let run_op = async |op: &DiffOp| -> Result<(), DbError> {
        let audit_id = match crate::audit::write_audit_row(
            backend.pool_handle().as_ref(),
            &app_id,
            &crate::audit::AuditRow {
                collection: op.collection.clone(),
                phase: crate::audit::Phase::Ddl,
                change_class: crate::audit::ChangeClass::from(op.class),
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
                // — this one is collection-slot).
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
            // ---------------------------------------------------------------
            // registerModel APPLIES NO SCHEMA CHANGE, so every schema-changing
            // kind is a contract violation by the time it reaches apply.
            //
            // `validate::changes_schema` refuses these before an ApprovedPlan
            // is ever built, which is what makes this arm unreachable. It is
            // NOT a second gate that could disagree with the first: there is no
            // DDL code left here to run, so the failure mode a drifting pair of
            // classifiers would produce - one allows, the other silently
            // no-ops - cannot occur. What it does is fail LOUDLY if an op ever
            // bypasses validate, instead of returning Ok and reporting a
            // deploy that changed nothing.
            //
            // Two of these look like data ops. `MaskBackfill` ends with
            // `ALTER COLUMN ... SET NOT NULL` and `MaskRemove` is an
            // `ALTER TABLE ... DROP COLUMN`; both are DDL. See
            // `validate::changes_schema`, which is the one place that judgement
            // is written down.
            // ---------------------------------------------------------------
            ChangeKind::CreateTable
            | ChangeKind::AddColumn
            | ChangeKind::AddIndex
            | ChangeKind::AddForeignKey
            | ChangeKind::DropForeignKey
            | ChangeKind::DropColumn
            | ChangeKind::DropIndex
            | ChangeKind::RewriteColumnType { .. }
            | ChangeKind::MaskBackfill { .. }
            | ChangeKind::MaskRemove { .. } => Err(schema_change_invariant_error(op)),

            // ----- Rewrite the sibling column ---------
            //
            // Touches every row under the NEW kind. No schema mutation - the
            // only op in the family that changes data without changing shape,
            // and therefore the only one apply still performs.
            ChangeKind::MaskRewrite {
                collection: ref coll,
                column,
                old_kind: _,
                new_kind,
                classification,
            } => {
                dispatch_mask_rewrite_op(
                    backend,
                    &app_id,
                    coll,
                    column,
                    *new_kind,
                    *classification,
                    &schema_json,
                )
                .await
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
                        // investigate stuck rows. Field shape is
                        // unified across all 8 F1 sites — 6
                        // audit_id-slot + 2 collection-slot variants.
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

    // ONE PASS. There used to be two.
    //
    // Pass 2 existed for `CREATE INDEX CONCURRENTLY`, and the advisory lock was
    // released between the passes because holding it through CIC deadlocks: a
    // second waiter blocked on `pg_advisory_lock` pins a snapshot CIC waits on.
    // registerModel no longer issues CIC - or any other DDL - so pass 2 had no
    // op it could ever select and the release-then-continue dance guarded a
    // hazard that cannot arise. Both are gone.
    //
    // No class filter either. `validate` refuses every op that changes shape,
    // whatever its `ChangeClass`, so the only op reaching this loop is a
    // `MaskRewrite`: an UPDATE over rows in a column that already exists. One
    // decision point, in validate, rather than a second partial one here.
    //
    // The loop still runs inside an async block so the lock is released on
    // EVERY path, early `?` included. Without that, `PooledClient::Drop` parks
    // the connection back in the pool with its session-scoped lock still held
    // and stalls every subsequent caller across apps.
    let ran: Result<(), DbError> = async {
        for op in &approved.ops {
            run_op(op).await?;
        }
        Ok(())
    }
    .await;

    // Best-effort: `release()` issues `pg_advisory_unlock` then returns the
    // now-unlocked client to the pool on drop, swallowing any SQL error. It
    // must run even when the loop above failed.
    let _ = lock_guard.release().await;

    ran?;

    Ok(())
}


async fn dispatch_mask_rewrite_op<B>(
    backend: &B,
    app_id: &str,
    collection: &str,
    column: &str,
    new_kind: crate::diff::MaskKind,
    classification: crate::diff::Classification,
    schema_json: &Value,
) -> Result<(), DbError>
where
    B: PgSqlExecutor + EncryptedColumn,
{
    let enc_meta = encryption_meta_for_field(schema_json, column);
    crate::crud::mask_backfill::run_mask_rewrite(
        backend,
        app_id,
        collection,
        column,
        new_kind,
        classification,
        enc_meta.as_ref(),
        backend.pool_handle().as_ref(),
    )
    .await
    .map(|_| ())
}


/// Pull the encryption metadata for a field out of the
/// declared schema JSON, IFF the field is `t.encrypted(...)`-tagged.
/// Returns `None` for plaintext columns; the mask backfill / rewrite
/// then takes the no-decrypt branch.
///
/// The shape mirrors `crud::encryption_pass::parse_mode` /
/// `parse_wraps` — kept independent so a tweak to the schema-wire
/// shape lands in one place per consumer (backfill vs CRUD).
fn encryption_meta_for_field(
    schema: &Value,
    field: &str,
) -> Option<crate::diff::EncryptionMeta> {
    let def = schema.as_object()?.get(field)?;
    let enc_obj = def.get("encrypted")?.as_object()?;
    let mode_str = enc_obj.get("mode").and_then(|v| v.as_str()).unwrap_or("randomised");
    let mode = match mode_str {
        "randomised" | "randomized" => crate::backend::EncryptionMode::Randomised,
        "deterministic" => crate::backend::EncryptionMode::Deterministic,
        _ => return None,
    };
    let key_id = enc_obj
        .get("keyId")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let wraps = match enc_obj.get("wraps").and_then(|v| v.as_str()) {
        Some("number") => crate::diff::WrappedType::Number,
        Some("bytes") => crate::diff::WrappedType::Bytes,
        _ => crate::diff::WrappedType::String,
    };
    Some(crate::diff::EncryptionMeta {
        mode,
        key_id,
        wraps,
    })
}

/// Build the diagnostic message for a destructive-invariant violation.
/// Named so the unreachable `DropColumn | DropIndex` arm inside the
/// `match` block can produce an identical error without duplicating
/// the message string.
/// A schema-changing op reached apply, which `validate` should have refused.
///
/// This replaces the older destructive-only invariant guard, which covered a
/// narrower rule: apply used to perform ADDITIVE DDL and only the destructive
/// family was a contract violation. Now NO op that changes shape may reach here,
/// so the guard covers the whole family and its message names the real
/// invariant.
///
/// It refuses rather than no-ops for the reason the old one did: returning `Ok`
/// would report a deploy that silently changed nothing.
fn schema_change_invariant_error(op: &DiffOp) -> DbError {
    let msg = format!(
        "db: apply received a {} op, which changes schema (class={:?}). \
         registerModel applies no schema change; validate::changes_schema \
         should have refused this before an ApprovedPlan was built. \
         This is a contract violation — refusing to silently no-op.",
        op.change_kind.as_sql(),
        op.class,
    );
    tracing::error!(
        change_kind = op.change_kind.as_sql(),
        class = ?op.class,
        collection = %op.collection,
        "apply: schema-change invariant violation"
    );
    DbError::Internal { message: msg }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the schema-change invariant and the audit warn shapes.
    //!
    //! SIX TESTS WERE DELETED HERE, not moved. They pinned a gate that checked
    //! whether a `DropColumn` / `DropIndex` / `MaskRemove` op carried
    //! `ChangeClass::Destructive` before apply skipped it. That gate is gone
    //! because its premise is: apply no longer performs any schema change under
    //! any class, so there is no skip-path for a misclassified op to slip past.
    //! The tests were asserting a distinction that no longer decides anything.
    //!
    //! These exercise pure helpers rather than `apply()` itself, which takes a
    //! `compio_postgres::PooledClient<'p>` and issues `pg_advisory_unlock`
    //! against it - end-to-end coverage needs a live Postgres and lives in
    //! `crates/zeroship-plugin-db/tests/integration.rs`.
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

    /// `MaskRemove` must reach apply ONLY tagged
    /// `Destructive`. The validate stage classifies it that way; this
    /// gate refuses any misclassified `MaskRemove`.
    #[test]
    fn schema_change_invariant_error_emits_named_fields_at_error_level() {
        use crate::test_support::capture;
        use tracing::Level;

        let op = diff_op(ChangeKind::DropColumn, ChangeClass::Additive);
        let (_err, events) = capture(|| schema_change_invariant_error(&op));

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
            ev.message.contains("schema-change invariant violation"),
            "message must name the contract for log-grep: {}",
            ev.message,
        );
    }

    /// Documentation snapshot of the F1 warn-half shape.
    ///
    /// Re-emits the same `tracing::warn!` syntax the F1 sites use in
    /// `apply.rs` (lines 178–184 / 209–216) and `backend/postgres.rs`
    /// (lines 496 / 548 / 597). Field names checked here MUST match the
    /// production sites; a runbook that greps for `audit_err=…` relies
    /// on every F1 site using that exact key.
    ///
    /// **What this test pins**: the shape written IN THIS FILE, against
    /// itself. It re-emits the `warn!` rather than calling production
    /// code, so the two copies are compared by a human, never by the
    /// compiler or the test runner.
    ///
    /// **What it does NOT catch, and this is the important half**: a
    /// rename at a PRODUCTION site alone. Nothing here reads
    /// `apply.rs`'s real warn arms, so `cargo test` stays green while
    /// the runbook's grep goes dead. It equally misses a rename made in
    /// both places at once. Driving the real path would need a Backend
    /// mock the brief scoped out; end-to-end coverage of the live F1
    /// path lives in `crates/plugin-db/tests/integration.rs`.
    ///
    /// So: treat this as documentation that happens to compile, not as
    /// a guard on the operator-grep contract.
    ///
    /// **What this test does NOT catch**: a contributor who renames
    /// `audit_err` → `error` in BOTH the production sites and this
    /// test in one commit. That class of drift is caught by the code
    /// review + the `transition` discriminator's literal-string match
    /// against the audit table's terminal-state grep.
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
    /// `validate.rs:101` + `apply.rs:84` running-row insert path).
    ///
    /// The original `f1_warn_shape_documentation_snapshot` above
    /// covers only the `audit_id` variant, so the `collection`-slot
    /// shape had no written-down form at all. This test supplies one.
    ///
    /// It has the SAME limit as its sibling, and the limit is easy to
    /// misread: like that test, this one re-emits the `warn!` rather
    /// than calling `validate.rs` or `apply.rs`. A rename of
    /// `collection` / `transition` / `audit_err` at either PRODUCTION
    /// site does NOT fail here - nothing compares the two copies. What
    /// fails here is a rename made in this file.
    #[test]
    fn f1_warn_shape_collection_slot_documentation_snapshot() {
        use crate::test_support::capture;
        use tracing::Level;

        // The exact syntax used at apply.rs:84 (the "Running/insert_failed"
        // arm). Mirrors the validate.rs:101 shape (transition =
        // "ValidationRefused/insert_failed").
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
