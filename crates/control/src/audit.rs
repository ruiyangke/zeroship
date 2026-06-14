//! Append-only audit log for sensitive operations.
//!
//! Every mutation of vars / secrets / Stripe accounts writes one row
//! here. Reads are intentionally NOT audited — that would 10x the
//! row volume on the hot worker-fetch path; if reads need to be
//! audited later, do it at the gateway layer with a sampling
//! middleware instead.
//!
use serde_json::{json, Value};
use uuid::Uuid;

use crate::registry::{Registry, RegistryError};

#[derive(Debug, Clone, Copy)]
pub enum Action {
    SetVar,
    DeleteVar,
    SetSecret,
    DeleteSecret,
    /// Set the per-app `process.env` expose list — names of secrets the
    /// creator has opted to surface in `process.env`. Audited so ops
    /// can answer "when did we let X out of the secret namespace."
    SetEnvExpose,
    /// A Connect Express account was MINTED on Stripe for a creator (the
    /// `onboard` handler created a new `acct_…`). Distinct from `LinkAccount`
    /// (which records the verified-link in `callback`) so the trail separates
    /// "account minted" from "account verified-linked" (m6).
    CreateAccount,
    LinkAccount,
    UnlinkAccount,
    RecordPayout,
    /// A spend-enforcement state transition for an app, emitted by the
    /// spend-reconcile cron (billing PR5). The detail JSON carries
    /// `{ from, to, spend_cents, limit_cents }`.
    SpendStateChange,
    /// A creator changed an app's spend-limit override via the M4 endpoint.
    SetSpendLimit,
    /// A creator finished the Checkout setup flow and now has a saved default
    /// PaymentMethod (`setup_intent.succeeded` webhook, billing PR6 Stream-1).
    SetupIntentSucceeded,
    /// A finalized infra-billing invoice could not be charged
    /// (`invoice.payment_failed` webhook, billing PR6 Stream-1).
    InvoicePaymentFailed,
    /// A creator payment/account-state transition (billing G2): the
    /// active→past_due→suspended→active dunning lifecycle. Written by the
    /// webhook (`invoice.payment_failed`/`invoice.paid`) and the dunning cron.
    /// The detail JSON carries `{ from, to, reason, creator_id }`.
    AccountStateChange,
    /// An operator created or updated a plan in the catalog via `PUT
    /// /api/plans/:id` (billing-v2 MINOR-1). The plan's FX/price is the
    /// highest-leverage money lever, so the write is audited with the actor +
    /// the new price model in the detail JSON.
    PlanUpserted,
    /// An operator archived a plan via `DELETE /api/plans/:id` (billing-v2
    /// MINOR-1). Audited with the actor + the plan id.
    PlanArchived,
    /// An operator set a creator's application-fee policy via `PUT
    /// /api/creators/:id/fee-policy` (billing G1, ISS-29). The fee is
    /// server-authoritative + operator-only — a creator may never lower it — so
    /// every change is audited with the actor + the new policy in the detail JSON.
    SetFeePolicy,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SetVar => "set_var",
            Self::DeleteVar => "delete_var",
            Self::SetSecret => "set_secret",
            Self::DeleteSecret => "delete_secret",
            Self::SetEnvExpose => "set_env_expose",
            Self::CreateAccount => "create_account",
            Self::LinkAccount => "link_account",
            Self::UnlinkAccount => "unlink_account",
            Self::RecordPayout => "record_payout",
            Self::SpendStateChange => "spend_state_change",
            Self::SetSpendLimit => "set_spend_limit",
            Self::SetupIntentSucceeded => "setup_intent_succeeded",
            Self::InvoicePaymentFailed => "invoice_payment_failed",
            Self::AccountStateChange => "account_state_change",
            Self::PlanUpserted => "plan_upserted",
            Self::PlanArchived => "plan_archived",
            Self::SetFeePolicy => "set_fee_policy",
        }
    }
}

pub struct AuditEntry<'a> {
    pub app_id: Option<Uuid>,
    pub creator_id: Option<Uuid>,
    pub actor_user_id: Option<Uuid>,
    pub actor_token_id: Option<Uuid>,
    pub action: Action,
    pub resource: Option<&'a str>,
    pub source_ip: Option<&'a str>,
}

/// Best-effort audit insert. We never fail the caller's operation just
/// because we couldn't write an audit row — if the DB is partially down
/// the user-facing op should still succeed and we log to stderr instead.
pub async fn log(registry: &Registry, entry: AuditEntry<'_>) {
    log_with_detail(registry, entry, &Value::Null).await;
}

/// Best-effort audit insert with structured detail JSON for operations where
/// `resource` alone is not enough to reconstruct the mutation.
pub async fn log_with_detail(registry: &Registry, entry: AuditEntry<'_>, detail: &Value) {
    let stdout_payload = json!({
        "app_id": entry.app_id,
        "creator_id": entry.creator_id,
        "actor_user_id": entry.actor_user_id,
        "actor_token_id": entry.actor_token_id,
        "action": entry.action.as_str(),
        "resource": entry.resource,
        "source_ip": entry.source_ip,
        "detail": detail,
    });
    tracing::info!(target: "control.audit", payload = %stdout_payload, "app audit event");

    let conn = match registry.conn().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "audit: connect failed");
            return;
        }
    };
    let result = conn
        .execute(
            // `$7::text::inet`: bind the param as TEXT (which `Option<&str>`
            // serializes as) and let PG cast text→inet, instead of `$7::inet`
            // which makes PG infer the param OID as `inet` and reject the `&str`
            // bind at serialize time ("error serializing parameter"). The latter
            // silently broke EVERY detail-audit insert (best-effort path).
            "INSERT INTO zeroship.app_audit(app_id, creator_id, actor_user_id, actor_token_id, action, resource, source_ip, detail)
             VALUES($1, $2, $3, $4, $5, $6, $7::text::inet, $8)",
            &[
                &entry.app_id,
                &entry.creator_id,
                &entry.actor_user_id,
                &entry.actor_token_id,
                &entry.action.as_str(),
                &entry.resource,
                &entry.source_ip,
                &detail,
            ],
        )
        .await;
    if let Err(e) = result {
        tracing::warn!(action = entry.action.as_str(), error = %e, "audit: insert failed");
    }
}

/// Read recent audit entries for an app. Newest first.
pub async fn recent_for_app(
    registry: &Registry,
    app_id: Uuid,
    limit: i64,
) -> Result<Vec<AuditRow>, RegistryError> {
    let limit = limit.clamp(1, 500);
    let conn = registry.conn().await?;
    let rows = conn
        .query(
            "SELECT id, actor_user_id, actor_token_id, action, resource, source_ip::text AS source_ip,
                    to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS at_text
             FROM zeroship.app_audit
             WHERE app_id = $1
             ORDER BY occurred_at DESC
             LIMIT $2",
            &[&app_id, &limit],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| AuditRow {
            id: r.get("id"),
            actor_user_id: r.get("actor_user_id"),
            actor_token_id: r.get("actor_token_id"),
            action: r.get("action"),
            resource: r.get("resource"),
            source_ip: r.get("source_ip"),
            at: r.get("at_text"),
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub id: Uuid,
    pub actor_user_id: Option<Uuid>,
    pub actor_token_id: Option<Uuid>,
    pub action: String,
    pub resource: Option<String>,
    pub source_ip: Option<String>,
    pub at: String,
}
