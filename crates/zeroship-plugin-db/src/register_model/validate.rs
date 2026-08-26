//! Stage 3 — Validate.
//!
//! Decides what, if anything, the apply stage may do to the [`Plan`] from
//! stage 2. In practice it refuses nearly everything, and that is the design.
//!
//! # The one rule that matters
//!
//! registerModel applies NO schema change. Any op for which
//! [`changes_schema`] is true is refused, under every strictness, with an
//! envelope naming the refused change. What survives is `MaskRewrite` - an
//! UPDATE recomputing a `_masked` sibling on a column that already exists.
//!
//! STRICTNESS NO LONGER GATES THIS. It used to: `strict` refused destructive
//! ops, `lenient` refused-but-continued, and `off` let them through into apply.
//! Those three answered "may we apply a risky change?", and there is no longer
//! a code path that applies one - so an escape hatch would only produce a deploy
//! that reports success and changes nothing. Strictness still governs the
//! separate destructive-class check below, which now only sees ops that change
//! no shape.
//!
//! Refused ops are still INSERTed terminal as `validation_refused` so operators
//! can see what was refused with no orphan-Pending row.
//!
//! Returns an [`ApprovedPlan`] the apply stage executes.
//!
//! ### Why this stage stays on `Result<_, String>`
//!
//! Every other pipeline stage now returns `Result<_, DbError>` (Stage
//! 8e final sweep). Validate is the lone exception: its only `Err` path
//! is the `validation_refused` JSON envelope, and the envelope itself
//! is the documented SDK wire contract — the SDK does
//! `JSON.parse(err.message)` to recover the structured refusal, so the
//! message body must reach JS byte-for-byte.
//!
//! `run_pipeline` wraps the `Err(envelope)` in
//! [`crate::error::DbError::SchemaRefused`] at the boundary; that
//! variant's `to_op_error()` arm stamps `.code` from the static
//! discriminator (typically `"validation_refused"`) AND emits the
//! envelope as the JS `Error.message`. SDK callers can branch on
//! `err.code === "validation_refused"` directly or `JSON.parse` the
//! message to recover the structured payload. Net result:
//! wire-compatible refusal flow + DbError-typed pipeline.

use serde_json::Value;

use super::bootstrap::RegisterContext;
use super::plan::Plan;
use crate::backend::AuditWriter;
use crate::diff::{ChangeClass, ChangeKind, DiffOp};

/// Does this op CHANGE THE SCHEMA, as opposed to only rewriting stored data?
///
/// registerModel does not own schema. It registers metadata; the schema
/// authority is a separate process (`crates/zeroship-migrated` at deploy, the
/// vite plugin's dev apply locally). Every op this returns `true` for is
/// therefore refused by [`validate`] rather than applied - see its doc for why
/// that is structural and not a strictness setting.
///
/// The match is EXHAUSTIVE on purpose. A new `ChangeKind` must not default into
/// either bucket: defaulting to `false` would silently hand plugin-db a new way
/// to mutate a creator's database, which is the exact failure this classifier
/// exists to prevent.
///
/// TWO ENTRIES LOOK LIKE DATA OPS AND ARE NOT. `MaskBackfill` walks rows writing
/// the sibling column, then finishes with `ALTER COLUMN ... SET NOT NULL`, and
/// `MaskRemove` is an `ALTER TABLE ... DROP COLUMN` outright (both in
/// `crud::mask_backfill`). Only `MaskRewrite` is genuinely DDL-free: it is an
/// UPDATE over an existing column and changes no shape.
fn changes_schema(kind: &ChangeKind) -> bool {
    match kind {
        ChangeKind::CreateTable
        | ChangeKind::AddColumn
        | ChangeKind::DropColumn
        | ChangeKind::AddIndex
        | ChangeKind::DropIndex
        | ChangeKind::AddForeignKey
        | ChangeKind::DropForeignKey
        | ChangeKind::RewriteColumnType { .. }
        | ChangeKind::MaskBackfill { .. }
        | ChangeKind::MaskRemove { .. } => true,
        ChangeKind::MaskRewrite { .. } => false,
    }
}

/// Output of stage 3. Apply still receives the destructive ops (kept
/// in `ops` so the loop's `if op.class == Destructive { continue; }`
/// gate is the single source of truth for "never apply destructive"),
/// but a `strict` deploy never reaches this branch — validate returns
/// `Err(envelope)` before constructing the `ApprovedPlan`.
#[derive(Debug)]
pub(crate) struct ApprovedPlan {
    pub ops: Vec<DiffOp>,
}

/// Run stage 3.
///
/// Best-effort audit writes — a failed insert to `__zeroship_migrations`
/// must NOT mask the validation_refused envelope. `tracing::warn` so it
/// shows up in worker logs but the user-facing error stays clean.
///
/// Best-effort audit writes route through [`AuditWriter`] so both the PG
/// and SQLite register pipelines can reuse this stage.
pub(crate) async fn validate<B: AuditWriter>(
    backend: &B,
    ctx: &RegisterContext,
    plan: Plan,
) -> Result<ApprovedPlan, String> {
    let mut reserved_collections: Vec<String> = plan
        .ops
        .iter()
        .map(|op| op.collection.clone())
        .filter(|collection| collection.starts_with("__zeroship_"))
        .collect();
    if ctx.collection.starts_with("__zeroship_") {
        reserved_collections.push(ctx.collection.clone());
    }
    reserved_collections.sort();
    reserved_collections.dedup();
    if !reserved_collections.is_empty() {
        return Err(build_reserved_prefix_refused_envelope(
            &ctx.deploy_id,
            &reserved_collections,
        ));
    }

    // ---------------------------------------------------------------------
    // registerModel APPLIES NO SCHEMA CHANGE. Refused before anything else.
    //
    // This is STRUCTURAL, not a safety judgement, which is why it does not
    // consult `strictness`. Strictness answered "may we apply a risky change?";
    // there is no longer a change to apply, because plugin-db has no DDL path
    // at all. A `lenient` deploy that fell through here would reach an apply
    // stage with nothing to execute it.
    //
    // The refusal is the point. Silently reconciling a creator's table on
    // deploy is the black box this replaces: the declared schema and the live
    // table drifted apart and the operator was never told. Now the deploy stops
    // and names the process that owns the change.
    // ---------------------------------------------------------------------
    let schema_changes: Vec<&DiffOp> = plan
        .ops
        .iter()
        .filter(|op| changes_schema(&op.change_kind))
        .collect();

    if !schema_changes.is_empty() {
        for op in &schema_changes {
            let row = crate::audit::AuditRow {
                collection: op.collection.clone(),
                phase: crate::audit::Phase::Ddl,
                change_class: crate::audit::ChangeClass::from(op.class),
                change_kind: op.change_kind.as_sql().to_string(),
                details: op.details.clone(),
                ddl_sql: op.sql.clone(),
                status: crate::audit::InitialStatus::ValidationRefused,
                deploy_id: ctx.deploy_id.clone(),
                schema_version: ctx.schema_version,
                actor: crate::audit::ActorKind::Auto,
            };
            if let Err(audit_err) = backend.write_audit_row(&ctx.app_id, &row).await {
                tracing::warn!(
                    app_id = %ctx.app_id,
                    collection = %op.collection,
                    transition = "ValidationRefused/insert_failed",
                    audit_err = %audit_err,
                    "audit: failed to log a refused schema change",
                );
            }
        }
        return Err(build_schema_change_refused_envelope(
            &ctx.deploy_id,
            &schema_changes,
        ));
    }

    let destructive: Vec<&DiffOp> = plan
        .ops
        .iter()
        .filter(|op| op.class == ChangeClass::Destructive)
        .collect();

    if !destructive.is_empty() && ctx.strictness != "off" {
        // Every audit row must reach a terminal status. An earlier shape
        // wrote `Pending` then drove it to `Failed` via a second UPDATE -
        // a two-statement transition that could leave a row stranded in
        // `pending` if the worker crashed or the UPDATE failed between
        // the two writes.
        //
        // The row instead lands directly terminal via INSERT with
        // `status = 'validation_refused'`. The audit table's
        // status CHECK now accepts `'validation_refused'` (extended in
        // `ensure_audit_table_exists`); both strict and lenient modes
        // route through this terminal — eliminating the orphan window
        // entirely, with no UPDATE round-trip to fail.
        for op in &destructive {
            let row = crate::audit::AuditRow {
                collection: op.collection.clone(),
                phase: crate::audit::Phase::Ddl,
                change_class: crate::audit::ChangeClass::from(op.class),
                change_kind: op.change_kind.as_sql().to_string(),
                details: op.details.clone(),
                ddl_sql: op.sql.clone(),
                status: crate::audit::InitialStatus::ValidationRefused,
                deploy_id: ctx.deploy_id.clone(),
                schema_version: ctx.schema_version,
                actor: crate::audit::ActorKind::Auto,
            };
            // Best-effort: a failure to write the audit row should not
            // mask the envelope - tracing::warn so it shows in worker
            // logs but the user-facing error stays clean. Field shape
            // matches the warn-half family (`app_id` + `audit_err`);
            // see `test_support` for the drift this field shape guards
            // against.
            if let Err(audit_err) = backend.write_audit_row(&ctx.app_id, &row).await {
                tracing::warn!(
                    app_id = %ctx.app_id,
                    collection = %op.collection,
                    transition = "ValidationRefused/insert_failed",
                    audit_err = %audit_err,
                    "audit: failed to log destructive op",
                );
            }
        }

        if ctx.strictness == "strict" {
            return Err(build_validation_refused_envelope(&ctx.deploy_id, &destructive));
        }
        // strictness == "lenient": fall through, but apply will skip
        // destructive ops. The audit row above already terminalised so
        // operators see the refusal without an orphan-Pending row.
    }

    // -------------------------------------------------------------------
    // Existence-check validation (proposal A2 line 153) is currently
    // delegated to:
    //   - the diff classifier (NOT NULL on non-empty table → Destructive)
    //   - `create_index_with_recovery_audited` (UNIQUE conflicts surface
    //     via 23505 during CIC)
    // The exhaustive validation budget loop is deferred to a follow-up
    // PR; for the additive-only flows the diff engine identifies,
    // classification already prevents unsafe DDL.
    // -------------------------------------------------------------------

    Ok(ApprovedPlan { ops: plan.ops })
}

/// Build the `validation_refused` error envelope (proposal A2 line 167).
/// The shape matches the SDK's expected error contract so the deploy
/// pipeline can render the failing PKs / approval URL uniformly.
fn build_validation_refused_envelope(
    deploy_id: &str,
    destructive: &[&DiffOp],
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

/// The envelope for "your declared schema needs a change registerModel cannot
/// make".
///
/// It keeps `code: "validation_refused"` so the SDK's existing
/// `JSON.parse(err.message)` contract and its `err.code` discriminator keep
/// working unchanged; a new code would break every creator handling the old one.
/// The `reason` field is what tells them apart, and `pending` lists exactly which
/// changes were refused - naming them is the whole point, since the failure this
/// replaces was silence.
fn build_schema_change_refused_envelope(deploy_id: &str, ops: &[&DiffOp]) -> String {
    let pending: Vec<Value> = ops
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
        "reason": "schema_change_requires_migration",
        "deploy_id": deploy_id,
        "message": "registerModel does not apply schema changes. The declared \
                    schema differs from the live table; run the migration \
                    process that owns this database and deploy again.",
        "violations": [],
        "schema_changes_refused": pending,
        "destructive_pending": [],
    })
    .to_string()
}

fn build_reserved_prefix_refused_envelope(
    deploy_id: &str,
    collections: &[String],
) -> String {
    let violations: Vec<Value> = collections
        .iter()
        .map(|collection| {
            serde_json::json!({
                "collection": collection,
                "code": "reserved_prefix",
                "message": "collection names starting with __zeroship_ are reserved",
            })
        })
        .collect();

    serde_json::json!({
        "code": "validation_refused",
        "deploy_id": deploy_id,
        "violations": violations,
        "destructive_pending": [],
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;

    #[derive(Default)]
    struct RecordingAuditWriter {
        statuses: RefCell<Vec<String>>,
        kinds: RefCell<Vec<String>>,
    }

    impl crate::backend::AuditWriter for RecordingAuditWriter {
        async fn ensure_audit_table(&self, _app_id: &str) -> Result<(), crate::error::DbError> {
            Ok(())
        }

        async fn next_schema_version(&self, _app_id: &str) -> Result<i32, crate::error::DbError> {
            Ok(1)
        }

        async fn write_audit_row_returning_id(
            &self,
            _app_id: &str,
            row: &crate::audit::AuditRow,
        ) -> Result<i64, crate::error::DbError> {
            self.statuses
                .borrow_mut()
                .push(row.status.as_sql().to_string());
            self.kinds
                .borrow_mut()
                .push(row.change_kind.clone());
            Ok(1)
        }

        async fn update_audit_status(
            &self,
            _app_id: &str,
            _audit_id: i64,
            _status: crate::audit::TerminalStatus,
            _error_message: Option<&str>,
        ) -> Result<bool, crate::error::DbError> {
            Ok(true)
        }
    }

    fn ctx(strictness: &str) -> RegisterContext {
        RegisterContext {
            app_id: "app_validate".to_string(),
            deploy_id: "deploy_validate".to_string(),
            schema_version: 7,
            strictness: strictness.to_string(),
            declared_indexes: Vec::new(),
            collection: "posts".to_string(),
            schema_json: json!({ "name": { "type": "string" } }),
        }
    }

    fn destructive_plan() -> Plan {
        Plan {
            ops: vec![DiffOp {
                collection: "posts".to_string(),
                change_kind: crate::diff::ChangeKind::DropColumn,
                class: crate::diff::ChangeClass::Destructive,
                sql: None,
                details: json!({}),
                field: Some("legacy_score".to_string()),
            }],
        }
    }

    fn additive_plan(collection: &str) -> Plan {
        Plan {
            ops: vec![DiffOp {
                collection: collection.to_string(),
                change_kind: crate::diff::ChangeKind::CreateTable,
                class: crate::diff::ChangeClass::Additive,
                sql: None,
                details: json!({}),
                field: None,
            }],
        }
    }

    /// A schema change is refused under EVERY strictness, `lenient` included.
    ///
    /// THIS TEST ASSERTED THE OPPOSITE until registerModel stopped applying
    /// schema. It pinned that a lenient deploy returned Ok and carried the op
    /// forward for apply to skip. That behaviour is the black box this replaces:
    /// the creator's declared schema and the live table disagreed, the deploy
    /// reported success, and nobody was told.
    ///
    /// Strictness is not consulted any more, and cannot be: it used to answer
    /// "may we apply a risky change?", and there is no longer a code path that
    /// applies one. A `lenient` escape hatch here would return Ok for a deploy
    /// whose table never changes.
    #[test]
    fn lenient_is_refused_too_because_the_refusal_is_structural() {
        let rt = compio::runtime::Runtime::new().expect("compio runtime");
        rt.block_on(async {
            let backend = RecordingAuditWriter::default();
            let envelope = validate(&backend, &ctx("lenient"), destructive_plan())
                .await
                .expect_err("lenient must NOT be an escape hatch for a schema change");

            assert!(
                envelope.contains("schema_change_requires_migration"),
                "the envelope must say WHY, not just that it refused: {envelope}"
            );
            assert!(
                envelope.contains("drop_column"),
                "the envelope must name the refused change so the operator can act: {envelope}"
            );
            // The audit row is still written terminal rather than left pending -
            // the operator-facing record survives the contract change.
            assert_eq!(
                backend.statuses.borrow().as_slice(),
                &["validation_refused".to_string()],
                "refused ops must be written terminal, not left pending"
            );
            assert_eq!(
                backend.kinds.borrow().as_slice(),
                &["drop_column".to_string()]
            );
        });
    }

    /// The same refusal for an ADDITIVE op, which used to be applied silently.
    ///
    /// `CreateTable` was the most common case: a first deploy found no table and
    /// registerModel created one. Additive ops never reached the old destructive
    /// filter at all, so nothing refused them and nothing logged them.
    #[test]
    fn an_additive_create_table_is_refused_and_named() {
        let rt = compio::runtime::Runtime::new().expect("compio runtime");
        rt.block_on(async {
            let backend = RecordingAuditWriter::default();
            let envelope = validate(&backend, &ctx("strict"), additive_plan("posts"))
                .await
                .expect_err("an additive CreateTable changes schema, so it is refused");

            assert!(
                envelope.contains("schema_change_requires_migration"),
                "additive refusals carry the same reason as destructive ones: {envelope}"
            );
            assert!(
                envelope.contains("create_table"),
                "the refused kind must be named: {envelope}"
            );
            assert_eq!(
                backend.kinds.borrow().as_slice(),
                &["create_table".to_string()],
                "an additive schema change is audited, which it never used to be"
            );
        });
    }

    #[test]
    fn reserved_zeroship_collection_prefix_is_refused() {
        let rt = compio::runtime::Runtime::new().expect("compio runtime");
        rt.block_on(async {
            let backend = RecordingAuditWriter::default();
            let envelope = match validate(
                &backend,
                &ctx("strict"),
                additive_plan("__zeroship_workflow_runs"),
            )
            .await
            {
                Ok(_) => panic!("reserved creator collection should be refused"),
                Err(envelope) => envelope,
            };
            let parsed: serde_json::Value =
                serde_json::from_str(&envelope).expect("validation envelope json");
            assert_eq!(parsed["code"], "validation_refused");
            assert_eq!(parsed["violations"][0]["code"], "reserved_prefix");
            assert_eq!(
                parsed["violations"][0]["collection"],
                "__zeroship_workflow_runs"
            );
            assert_eq!(
                parsed["destructive_pending"]
                    .as_array()
                    .expect("destructive_pending array")
                    .len(),
                0
            );
        });
    }
}
