//! Append-only audit log for sensitive operations.
//!
//! Every mutation of vars / secrets / Stripe accounts writes one row
//! here. Reads are intentionally NOT audited — that would 10x the
//! row volume on the hot worker-fetch path; if reads need to be
//! audited later, do it at the gateway layer with a sampling
//! middleware instead.
//!
use serde_json::Value;
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
    LinkAccount,
    UnlinkAccount,
    RecordPayout,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SetVar => "set_var",
            Self::DeleteVar => "delete_var",
            Self::SetSecret => "set_secret",
            Self::DeleteSecret => "delete_secret",
            Self::SetEnvExpose => "set_env_expose",
            Self::LinkAccount => "link_account",
            Self::UnlinkAccount => "unlink_account",
            Self::RecordPayout => "record_payout",
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
    let conn = match registry.conn().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "audit: connect failed");
            return;
        }
    };
    let result = conn
        .execute(
            "INSERT INTO app_audit(
                app_id, creator_id, actor_user_id, actor_token_id, action, resource, source_ip, detail
             )
             VALUES($1, $2, $3, $4, $5, $6, $7, $8)",
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
            "SELECT id, actor_user_id, actor_token_id, action, resource, source_ip,
                    to_char(at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS at_text
             FROM app_audit
             WHERE app_id = $1
             ORDER BY at DESC
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
