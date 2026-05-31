//! Structured audit-event emission.
//!
//! Every event lands in both: PG `zeroship.audit_events` (for query/retention)
//! and stdout JSON (for SIEM ingestion, per proposal §15).

use compio_postgres::{Client, GenericClient};
use ntex::web::HttpRequest;
use serde_json::{json, Value};

use crate::error::Result;
use crate::headers::RequestContext;
use crate::store::audit as store;

#[derive(Debug, Default)]
pub struct AuditEvent<'a> {
    pub event_type: &'a str,
    pub outcome: &'a str, // "success" | "failure"
    pub user_id: Option<&'a uuid::Uuid>,
    pub client_id: Option<&'a str>,
    pub request_id: Option<String>,
    pub ip: Option<std::net::IpAddr>,
    pub user_agent: Option<String>,
    pub auth_method: Option<&'a str>,
    pub detail: Value,
}

impl<'a> AuditEvent<'a> {
    #[must_use]
    pub fn from_request(req: &HttpRequest) -> Self {
        let ctx = {
            let extensions = req.extensions();
            extensions.get::<RequestContext>().cloned()
        }
        .unwrap_or_else(|| RequestContext::from_http_request(req));
        Self {
            request_id: Some(ctx.request_id),
            ip: ctx.ip,
            user_agent: ctx.user_agent,
            ..Self::default()
        }
    }
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
        "request_id":   ev.request_id.as_deref(),
        "ip":           ev.ip.map(|i| i.to_string()),
        "user_agent":   ev.user_agent.as_deref(),
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
        ev.request_id.as_deref(),
        ev.ip,
        ev.user_agent.as_deref(),
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
        "type":         ev.event_type,
        "outcome":      ev.outcome,
        "user_id":      ev.user_id,
        "client_id":    ev.client_id,
        "request_id":   ev.request_id.as_deref(),
        "ip":           ev.ip.map(|i| i.to_string()),
        "user_agent":   ev.user_agent.as_deref(),
        "auth_method":  ev.auth_method,
        "detail":       ev.detail,
    });
    tracing::info!(target: "auth.audit", payload = %stdout_payload, "audit event (strict)");

    store::insert(
        conn,
        ev.event_type,
        ev.outcome,
        ev.user_id,
        ev.client_id,
        ev.request_id.as_deref(),
        ev.ip,
        ev.user_agent.as_deref(),
        ev.auth_method,
        &ev.detail,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use ntex::web::test;

    use super::AuditEvent;

    #[test]
    fn from_request_prefills_request_metadata() {
        let req = test::TestRequest::default()
            .header("x-request-id", "req_h1")
            .header("x-forwarded-for", "203.0.113.42")
            .header("user-agent", "zeroship-test/1")
            .to_http_request();

        let ev = AuditEvent::from_request(&req);

        assert_eq!(ev.request_id.as_deref(), Some("req_h1"));
        assert_eq!(ev.ip, Some("203.0.113.42".parse::<IpAddr>().unwrap()));
        assert_eq!(ev.user_agent.as_deref(), Some("zeroship-test/1"));
    }

    #[test]
    fn from_request_generates_request_id_when_missing() {
        let req = test::TestRequest::default().to_http_request();
        let ev = AuditEvent::from_request(&req);

        assert!(ev.request_id.as_deref().is_some_and(|id| !id.is_empty()));
    }
}
