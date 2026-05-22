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
//! variant's `to_op_error()` arm explicitly does NOT add `.code` to
//! the JS exception (the envelope already carries
//! `"code":"validation_refused"` inside its JSON body). Net result:
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
                deploy_id: ctx.deploy_id.clone(),
                schema_version: ctx.schema_version,
                actor: crate::audit::ActorKind::Auto,
            };
            // Best-effort: a failure to write the audit row should not
            // mask the envelope — tracing::warn so it shows in worker
            // logs but the user-facing error stays clean.
            if let Err(e) = backend.write_audit_row(&ctx.app_id, &row).await {
                tracing::warn!(error = ?e, "audit: failed to log destructive op");
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
