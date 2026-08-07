//! Stage 3 — Validate.
//!
//! Applies safety rules to the [`Plan`] from stage 2:
//!
//! - `strictness == "strict"` (default): destructive ops produce a
//!   `validation_refused` envelope and short-circuit the pipeline.
//!   Every refused destructive op is INSERTed terminal as
//!   `validation_refused` so operators can see what was refused (no
//!   orphan-Pending row).
//! - `strictness == "lenient"`: destructive ops are INSERTed terminal
//!   as `validation_refused` and silently dropped from the apply set.
//! - `strictness == "off"`: destructive ops fall through into apply
//!   (this is the test/CI mode — proposal A2 line 122).
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
use crate::diff::{ChangeClass, DiffOp};

/// Output of stage 3. Apply still receives the destructive ops (kept
/// in `ops` so the loop's `if op.class == Destructive { continue; }`
/// gate is the single source of truth for "never apply destructive"),
/// but a `strict` deploy never reaches this branch — validate returns
/// `Err(envelope)` before constructing the `ApprovedPlan`.
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

    #[test]
    fn lenient_validation_terminalises_refused_destructive_ops() {
        let rt = compio::runtime::Runtime::new().expect("compio runtime");
        rt.block_on(async {
            let backend = RecordingAuditWriter::default();
            let approved = validate(&backend, &ctx("lenient"), destructive_plan())
                .await
                .expect("lenient deploy should not return a refusal envelope");

            assert_eq!(approved.ops.len(), 1, "lenient keeps the op for apply-time skipping");
            assert_eq!(
                backend.statuses.borrow().as_slice(),
                &["validation_refused".to_string()],
                "lenient destructive ops must be written terminal, not left pending"
            );
            assert_eq!(
                backend.kinds.borrow().as_slice(),
                &["drop_column".to_string()]
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
