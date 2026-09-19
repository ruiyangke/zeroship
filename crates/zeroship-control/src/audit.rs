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
use zeroship_core::{AppId, UserId};

use crate::registry::{Registry, RegistryError};

#[derive(Debug, Clone, Copy)]
pub enum Action {
    SetVar,
    DeleteVar,
    SetSecret,
    DeleteSecret,
    /// A creator wrote one raw-TCP egress rule for their app
    /// (`POST /api/apps/{id}/egress-rules`). The row is what the registry
    /// projects into every runtime, so who changed an app's reach and in
    /// which direction is the question this entry answers - the resource
    /// carries the verdict for exactly that reason.
    SetAppEgressRule,
    /// A creator removed one raw-TCP egress rule
    /// (`DELETE /api/apps/{id}/egress-rules`).
    DeleteAppEgressRule,
    /// Set the per-app `process.env` expose list — names of secrets the
    /// creator has opted to surface in `process.env`. Audited so ops
    /// can answer "when did we let X out of the secret namespace."
    SetEnvExpose,
    /// A Connect Express account was MINTED on Stripe for an organization (the
    /// `onboard` handler created a new `acct_…`). Distinct from `LinkAccount`
    /// (which records the verified-link in `callback`) so the trail separates
    /// "account minted" from "account verified-linked" (m6).
    CreateAccount,
    LinkAccount,
    UnlinkAccount,
    RecordPayout,
    /// A spend-enforcement state transition for an app, emitted by the
    /// spend-reconcile cron. The detail JSON carries
    /// `{ from, to, spend_cents, limit_cents }`.
    SpendStateChange,
    /// A creator changed an app's spend-limit override.
    SetSpendLimit,
    /// A creator finished the Checkout setup flow and now has a saved default
    /// PaymentMethod (`setup_intent.succeeded` webhook).
    SetupIntentSucceeded,
    /// A finalized infra-billing invoice could not be charged
    /// (`invoice.payment_failed` webhook).
    InvoicePaymentFailed,
    /// An organization payment/account-state transition (billing G2): the
    /// active→past_due→suspended→active dunning lifecycle. Written by the
    /// webhook (`invoice.payment_failed`/`invoice.paid`) and the dunning cron.
    /// The detail JSON carries `{ from, to, reason, organization_id }`.
    AccountStateChange,
    /// An operator created or updated a plan in the catalog via `PUT
    /// /api/plans/:id` (billing-v2 MINOR-1). The plan's FX/price is the
    /// highest-leverage money lever, so the write is audited with the actor +
    /// the new price model in the detail JSON.
    PlanUpserted,
    /// An operator archived a plan via `DELETE /api/plans/:id` (billing-v2
    /// MINOR-1). Audited with the actor + the plan id.
    PlanArchived,
    /// An operator set an organization's application-fee policy via `PUT
    /// /api/organizations/:id/fee-policy` (billing G1, ISS-29). The fee is
    /// server-authoritative + operator-only — an organization may never lower it — so
    /// every change is audited with the actor + the new policy in the detail JSON.
    SetFeePolicy,
    /// An operator changed the GLOBAL default FX via `PUT /api/pricing-config`.
    /// The global FX is the highest-leverage money lever — it reprices
    /// every plan that inherits (`fx == None`) — so the write is audited with the
    /// actor + the old→new value in the detail JSON.
    SetGlobalFx,
    /// An operator granted an organization credit via `POST /api/billing/credit`
    /// (PR-2). Credit is a money lever (it reduces a future
    /// bill), operator-only, so every grant is audited with the actor + the
    /// organization / amount / kind in the detail JSON.
    CreditGranted,
    /// An operator refunded a finalized invoice via `POST
    /// /api/invoices/{id}/refunds` (PR-3). A refund moves
    /// real money (a Stripe `Refund` for `destination='cash'`) or grants
    /// platform credit (`destination='credit'`), operator-only, so every refund is
    /// audited with the actor + the invoice / amount / destination in the detail JSON.
    InvoiceRefunded,
    /// An operator voided a finalized invoice via `POST /api/invoices/{id}/void`
    /// (PR-3). A void is the only legal finalized→void
    /// correction transition; it restores consumed credit (`void_reversal`) and
    /// reissues a corrected invoice, so it is audited with the actor + the
    /// voided/reissued ids + any true-up refund in the detail JSON.
    InvoiceVoided,
    /// A chargeback/dispute lifecycle event was recorded from a `charge.dispute.*`
    /// webhook (PR-8). A dispute claws back cash the cardholder
    /// paid — a forced reversal recorded as a `billing_disputes` row + a signed
    /// `invoice_payments` row — so every dispute create/resolve is audited with the
    /// dispute / invoice / amount / status in the detail JSON.
    RecordDispute,
    /// A refund we recorded `issued` later FAILED/CANCELED at Stripe
    /// (`charge.refund.updated`, webhook follow-up). The cash did not return to the
    /// cardholder, so the refund is reversed (status→failed; a credit-destination grant
    /// clawed back). Audited with the refund / terminal status / clawback in the detail JSON.
    RefundFailed,
    /// A payout to an organization's connected account FAILED (`payout.failed`, webhook
    /// follow-up). Audited with the organization / payout / amount / failure code.
    PayoutFailed,
    /// An end-user's Connect checkout charge FAILED (`payment_intent.payment_failed`,
    /// webhook follow-up). Informational; audited with the organization / PI / amount.
    CheckoutFailed,
    // -- Authority changes -------------------------------------------------
    //
    // Every entry below is written by `crate::organizations` INSIDE the
    // transaction that performs the effect, through [`log_in_tx`], and carries
    // the organization's typed id in `resource` with `app_id` NULL. They are
    // the durable answer to "who holds authority here, and who granted it" -
    // the one question a membership system exists to answer after the fact.
    /// An organization was minted, seating its creator as the first owner.
    OrganizationCreated,
    /// An organization's name, slug or billing address changed.
    OrganizationUpdated,
    /// A member was seated in an organization.
    OrganizationMemberAdded,
    /// A member's organization role changed. The detail carries `from` and `to`.
    OrganizationMemberRoleChanged,
    /// A member was removed from an organization by someone who outranked them.
    OrganizationMemberRemoved,
    /// A member gave up their OWN seat. Distinct from
    /// [`Self::OrganizationMemberRemoved`] because the two answer different
    /// questions after the fact - one says who was ejected and by whom, the
    /// other says who walked out - and a single action name would make the
    /// difference unrecoverable from the trail.
    OrganizationMemberLeft,
    /// An organization was closed. It is a soft close: the row, its members,
    /// its invitations and its billing history all survive, and nothing about
    /// it can be changed afterwards.
    OrganizationDissolved,
    /// Ownership moved to another member and the previous owner stepped down.
    /// `became_shared` records whether this also converted a personal
    /// organization into a shared one.
    OrganizationOwnershipTransferred,
    /// An invitation was issued. The token is NEVER in the detail - only its
    /// digest is stored anywhere, and this row names the invite by its typed id.
    OrganizationInviteCreated,
    /// An unconsumed invitation was revoked.
    OrganizationInviteRevoked,
    /// An invitation was redeemed and the redeemer seated.
    OrganizationInviteRedeemed,
    /// A project was created inside an organization.
    ProjectCreated,
    /// A project's name or slug changed.
    ProjectUpdated,
    /// A project was deleted. Hard, unlike an organization: a project names no
    /// money record, so nothing has to outlive it.
    ProjectDeleted,
    /// An app was deleted - the terminal end of the app lifecycle and the last
    /// step of the account-closure funnel.
    ///
    /// Written by `crate::organizations::delete_app` through [`log_in_tx`], so
    /// it shares the fate of the marker it describes, and it carries the app's
    /// typed id in `app_id` with the project it LEFT in `resource`. That pairing
    /// is the point: deletion detaches the app from its project, so afterwards
    /// this row is the only place that says which project it belonged to.
    AppDeleted,
    /// A member was seated on one project (the per-project narrowing grant).
    ProjectMemberAdded,
    /// A project seat moved to another role, in one statement rather than as a
    /// removal followed by a grant.
    ProjectMemberRoleChanged,
    /// A project seat was withdrawn.
    ProjectMemberRemoved,
    /// A database was created in a project and placed on a cluster. The detail
    /// carries the datastore and the zone, because placement is the decision
    /// nothing else records: the row says where it landed, and this says when
    /// and by whom it was put there.
    DatabaseCreated,
    /// A database was deleted. Only reachable once no app binds it, so this row
    /// is also the record that the last binding had already gone.
    DatabaseDeleted,
    /// An app was granted access to a database at one capability.
    ///
    /// A BINDING IS AN AUTHORITY CHANGE, which is why it is audited in the
    /// caller's transaction beside the seat changes rather than best-effort
    /// afterwards: it is the whole answer to "which app may read this data".
    DatabaseBound,
    /// An app's access to a database was withdrawn.
    DatabaseUnbound,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SetVar => "set_var",
            Self::DeleteVar => "delete_var",
            Self::SetSecret => "set_secret",
            Self::DeleteSecret => "delete_secret",
            Self::SetAppEgressRule => "set_app_egress_rule",
            Self::DeleteAppEgressRule => "delete_app_egress_rule",
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
            Self::SetGlobalFx => "set_global_fx",
            Self::CreditGranted => "credit_granted",
            Self::InvoiceRefunded => "invoice_refunded",
            Self::InvoiceVoided => "invoice_voided",
            Self::RecordDispute => "record_dispute",
            Self::RefundFailed => "refund_failed",
            Self::PayoutFailed => "payout_failed",
            Self::CheckoutFailed => "checkout_failed",
            Self::OrganizationCreated => "organization_created",
            Self::OrganizationUpdated => "organization_updated",
            Self::OrganizationMemberAdded => "organization_member_added",
            Self::OrganizationMemberRoleChanged => "organization_member_role_changed",
            Self::OrganizationMemberRemoved => "organization_member_removed",
            Self::OrganizationMemberLeft => "organization_member_left",
            Self::OrganizationDissolved => "organization_dissolved",
            Self::OrganizationOwnershipTransferred => "organization_ownership_transferred",
            Self::OrganizationInviteCreated => "organization_invite_created",
            Self::OrganizationInviteRevoked => "organization_invite_revoked",
            Self::OrganizationInviteRedeemed => "organization_invite_redeemed",
            Self::ProjectCreated => "project_created",
            Self::ProjectUpdated => "project_updated",
            Self::ProjectDeleted => "project_deleted",
            Self::AppDeleted => "app_deleted",
            Self::ProjectMemberAdded => "project_member_added",
            Self::ProjectMemberRoleChanged => "project_member_role_changed",
            Self::ProjectMemberRemoved => "project_member_removed",
            Self::DatabaseCreated => "database_created",
            Self::DatabaseDeleted => "database_deleted",
            Self::DatabaseBound => "database_bound",
            Self::DatabaseUnbound => "database_unbound",
        }
    }
}

#[derive(Debug)]
pub struct AuditEntry<'a> {
    pub app_id: Option<&'a AppId>,
    /// The organization the event is attributed to (`org_…`).
    ///
    /// Borrowed, not owned, for the same reason `resource` is: an audit entry is
    /// built at a call site that already holds the id and is consumed
    /// immediately. `app_audit.organization_id` is deliberately key-less and
    /// nullable - an audit row must stay readable after the organization it
    /// names is gone.
    pub organization_id: Option<&'a str>,
    pub actor_user_id: Option<&'a UserId>,
    pub action: Action,
    pub resource: Option<&'a str>,
    pub source_ip: Option<&'a str>,
}

/// Best-effort audit insert. We never fail the caller's operation just
/// because we couldn't write an audit row — if the DB is partially down
/// the user-facing op should still succeed and we log to stderr instead.
///
/// # The row is NOT atomic with the mutation it describes, and that is a gap
///
/// Callers mutate first and audit afterwards, on a separate connection and
/// outside any transaction with the mutation. Two failure shapes follow. A
/// failed insert leaves the stdout line below plus a `warn` - degraded but
/// detectable. A process death between the mutation committing and this
/// function being entered records NOTHING, not even the stdout line, because
/// the emit happens in here.
///
/// This is not a trail that only operators read out of band. `recent_for_app`
/// below backs `GET /api/apps/:id/audit`, so a caller of that endpoint is
/// served these rows directly and can be shown a mutation-free history for a
/// mutation that happened. Closing the window means threading a transaction
/// through `EnvStore` so the mutation and its audit row commit together, which
/// touches every env-mutation path.
///
/// The stdout emit deliberately precedes all database work, so the trail
/// survives a database that is down entirely. That mitigates the first shape
/// and not the second.
pub async fn log(registry: &Registry, entry: AuditEntry<'_>) {
    log_with_detail(registry, entry, &Value::Null).await;
}

/// Best-effort audit insert with structured detail JSON for operations where
/// `resource` alone is not enough to reconstruct the mutation.
pub async fn log_with_detail(registry: &Registry, entry: AuditEntry<'_>, detail: &Value) {
    let app_id = entry.app_id.map(AppId::as_str);
    let actor_user_id = entry.actor_user_id.map(UserId::as_str);
    let stdout_payload = json!({
        "app_id": app_id,
        "organization_id": entry.organization_id,
        "actor_user_id": actor_user_id,
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
            "INSERT INTO zeroship.app_audit(app_id, organization_id, actor_user_id, action, resource, source_ip, detail)
             VALUES($1, $2, $3, $4, $5, $6::text::inet, $7)",
            &[
                &app_id,
                &entry.organization_id,
                &actor_user_id,
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

/// Write one audit row on the caller's OWN connection or transaction.
///
/// # Why this exists beside [`log_with_detail`], which opens its own connection
///
/// The two placements answer different questions and neither is right for both.
///
/// An env or Stripe mutation is audited AFTER it commits, on a separate
/// connection, because the trail must survive even when the mutation's own
/// transaction is long gone - and because those paths have no transaction to
/// join. That is [`log`] and [`log_with_detail`], and their header describes the
/// window they leave open.
///
/// An AUTHORITY CHANGE is the opposite case. "Who was made an owner" is only
/// true if the row that made them one committed, so the audit row must share
/// that fate: an effect that rolled back did not happen, and a trail claiming
/// otherwise is worse than no trail at all. Passing the caller's `Transaction`
/// here is what ties the two together.
///
/// The DECISION row is a third thing and goes the other way again -
/// `zeroship_authz::enforce` writes `zeroship.authz_decisions` on the shared
/// client, because a refused mutation rolls back and would take the record of
/// its own refusal with it.
///
/// Still best-effort: a failed insert warns rather than failing the caller's
/// mutation. The difference from [`log_with_detail`] is placement, not
/// severity.
pub async fn log_in_tx<C: compio_postgres::GenericClient + Sync>(
    conn: &C,
    entry: AuditEntry<'_>,
    detail: &Value,
) {
    let app_id = entry.app_id.map(AppId::as_str);
    let actor_user_id = entry.actor_user_id.map(UserId::as_str);
    let stdout_payload = json!({
        "app_id": app_id,
        "organization_id": entry.organization_id,
        "actor_user_id": actor_user_id,
        "action": entry.action.as_str(),
        "resource": entry.resource,
        "source_ip": entry.source_ip,
        "detail": detail,
    });
    tracing::info!(target: "control.audit", payload = %stdout_payload, "authority change");

    let result = conn
        .execute(
            // `$6::text::inet` for the reason `log_with_detail` states: binding
            // an `Option<&str>` against an inferred `inet` OID fails at
            // serialize time.
            "INSERT INTO zeroship.app_audit(app_id, organization_id, actor_user_id, action, resource, source_ip, detail)
             VALUES($1, $2, $3, $4, $5, $6::text::inet, $7)",
            &[
                &app_id,
                &entry.organization_id,
                &actor_user_id,
                &entry.action.as_str(),
                &entry.resource,
                &entry.source_ip,
                &detail,
            ],
        )
        .await;
    if let Err(e) = result {
        tracing::warn!(action = entry.action.as_str(), error = %e, "audit: in-transaction insert failed");
    }
}

/// Read recent audit entries for an app. Newest first.
pub async fn recent_for_app(
    registry: &Registry,
    app_id: &AppId,
    limit: i64,
) -> Result<Vec<AuditRow>, RegistryError> {
    let limit = limit.clamp(1, 500);
    let conn = registry.conn().await?;
    let rows = conn
        .query(
            "SELECT id, actor_user_id, action, resource, source_ip::text AS source_ip,
                    to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS at_text
             FROM zeroship.app_audit
             WHERE app_id = $1
             ORDER BY occurred_at DESC
             LIMIT $2",
            &[&app_id.as_str(), &limit],
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok(AuditRow {
                id: row.get("id"),
                actor_user_id: crate::user_id::optional_from_row(
                    row,
                    "actor_user_id",
                    "read app audit",
                )?,
                action: row.get("action"),
                resource: row.get("resource"),
                source_ip: row.get("source_ip"),
                at: row.get("at_text"),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub id: Uuid,
    pub actor_user_id: Option<UserId>,
    pub action: String,
    pub resource: Option<String>,
    pub source_ip: Option<String>,
    pub at: String,
}
