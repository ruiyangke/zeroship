//! Structured audit-event emission.
//!
//! Every event lands in both: PG `auth.audit_events` (for query/retention)
//! and stdout JSON (for SIEM ingestion, per proposal §15).

use compio_postgres::{Client, GenericClient};
use serde_json::{json, Value};

use crate::error::Result;
use crate::store::audit as store;

#[derive(Debug, Default)]
pub struct AuditEvent<'a> {
    pub event_type: &'a str,
    pub outcome: &'a str, // "success" | "failure"
    pub user_id: Option<&'a uuid::Uuid>,
    pub client_id: Option<&'a str>,
    pub request_id: Option<&'a str>,
    pub ip: Option<std::net::IpAddr>,
    pub user_agent: Option<&'a str>,
    pub auth_method: Option<&'a str>,
    pub detail: Value,
}

/// Emit an audit event. Failure to insert is logged but does NOT propagate —
/// audit must never block the user-facing request.
pub async fn emit(conn: &Client, ev: &AuditEvent<'_>) {
    // stdout fan-out first (cheap, can't fail).
    let stdout_payload = json!({
        "type":         ev.event_type,
        "outcome":      ev.outcome,
        "user_id":      ev.user_id,
        "client_id":    ev.client_id,
        "request_id":   ev.request_id,
        "ip":           ev.ip.map(|i| i.to_string()),
        "user_agent":   ev.user_agent,
        "auth_method":  ev.auth_method,
        "detail":       ev.detail,
    });
    tracing::info!(target: "auth.audit", payload = %stdout_payload, "audit event");

    // PG insert.
    if let Err(e) = store::insert(
        conn,
        ev.event_type,
        ev.outcome,
        ev.user_id,
        ev.client_id,
        ev.request_id,
        ev.ip,
        ev.user_agent,
        ev.auth_method,
        &ev.detail,
    )
    .await
    {
        tracing::error!(error = %e, event_type = ev.event_type, "audit PG insert failed");
    }
}

/// Strict variant — propagates PG insert errors instead of swallowing them.
/// Use for security-critical state transitions (e.g., password reset, role
/// grant) where a missing audit row IS a real correctness failure.
///
/// # Errors
///
/// Returns `AuthError::Db` (via the `store::insert` propagation) on PG insert failure.
pub async fn emit_strict(conn: &(impl GenericClient + Sync), ev: &AuditEvent<'_>) -> Result<()> {
    let stdout_payload = json!({
        "type": ev.event_type, "outcome": ev.outcome,
        "user_id": ev.user_id, "client_id": ev.client_id,
        "detail": ev.detail,
    });
    tracing::info!(target: "auth.audit", payload = %stdout_payload, "audit event (strict)");

    store::insert(
        conn,
        ev.event_type,
        ev.outcome,
        ev.user_id,
        ev.client_id,
        ev.request_id,
        ev.ip,
        ev.user_agent,
        ev.auth_method,
        &ev.detail,
    )
    .await
}
