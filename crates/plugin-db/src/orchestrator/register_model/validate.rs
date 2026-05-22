//! Stage 3 — Validate.
//!
//! Applies safety rules to the [`Plan`] from stage 2:
//!
//! - `strictness == "strict"` (default): destructive ops produce a
//!   `validation_refused` envelope and short-circuit the pipeline.
//!   Every refused destructive op is still audited as `pending` so
//!   operators can see what was refused.
//! - `strictness == "lenient"`: destructive ops are audited as
//!   `pending` and silently dropped from the apply set.
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
use crate::backend::Backend;
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
pub(crate) async fn validate<B: Backend>(
    backend: &B,
    ctx: &RegisterContext,
    plan: Plan,
) -> Result<ApprovedPlan, String> {
    let destructive: Vec<&DiffOp> = plan
        .ops
        .iter()
        .filter(|op| op.class == ChangeClass::Destructive)
        .collect();

    if !destructive.is_empty() && ctx.strictness != "off" {
        // F2 resolution (migration-pipeline r2 §F2; 4+ cycle carry):
        // every audit row must reach a terminal status. Previously the
        // destructive-op Pending row was written here and then ORPHANED —
        // strict mode short-circuited via the envelope without touching
        // the row, and lenient mode let `apply` skip the destructive op
        // without terminalising it. Operators querying
        // `status = 'pending'` saw phantom in-flight work that never
        // resolved.
        //
        // The fix writes the Pending row AND immediately drives it to
        // `Failed` with `validation_refused` as the error message
        // marker. We use `Failed` (not a new `ValidationRefused`
        // terminal) because the audit table's status CHECK constraint
        // (`audit.rs:218-220`) doesn't include `validation_refused`,
        // and rolling a new terminal across existing tables would need
        // a CHECK ALTER on every app's `__zeroship_migrations` — out
        // of scope for a refactor-safe pickup. The error_message
        // marker is the distinguishable token (`validation_refused`)
        // operators grep on, the change_class column carries
        // `destructive`, and the deploy_id ties the row to the rejected
        // deploy. The design doc's "new ValidationRefused terminal"
        // recommendation is preserved as future work gated on a
        // coordinated CHECK rewrite.
        for op in &destructive {
            let row = crate::audit::AuditRow {
                collection: op.collection.clone(),
                phase: crate::audit::Phase::Ddl,
                change_class: op.class.as_audit(),
                change_kind: op.change_kind.as_sql().to_string(),
                details: op.details.clone(),
                ddl_sql: op.sql.clone(),
                status: crate::audit::InitialStatus::Pending,
                deploy_id: ctx.deploy_id.clone(),
                schema_version: ctx.schema_version,
                actor: crate::audit::ActorKind::Auto,
            };
            // Best-effort: a failure to write the audit row should not
            // mask the envelope — tracing::warn so it shows in worker
            // logs but the user-facing error stays clean.
            match backend.write_audit_row(&ctx.app_id, &row).await {
                Ok(audit_id) => {
                    // Terminalise immediately to close the orphan-Pending
                    // window (F2). Field shape matches the F1 warn-half
                    // family (`audit_err`, `transition`) so operators
                    // grep the same emissions.
                    if let Err(audit_err) = backend
                        .update_audit_status(
                            &ctx.app_id,
                            audit_id,
                            crate::audit::TerminalStatus::Failed,
                            Some("validation_refused"),
                        )
                        .await
                    {
                        tracing::warn!(
                            app_id = %ctx.app_id,
                            audit_id = audit_id,
                            transition = "Failed/validation_refused",
                            audit_err = %audit_err,
                            "update_audit_status failed; row stays in 'pending' until reset",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "audit: failed to log destructive op");
                }
            }
        }

        if ctx.strictness == "strict" {
            return Err(build_validation_refused_envelope(&ctx.deploy_id, &destructive));
        }
        // strictness == "lenient": fall through, but apply will skip
        // destructive ops.
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
