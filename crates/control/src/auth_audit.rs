//! Control-plane events that land in `auth.audit_events`.

use ntex::web;
use serde_json::{json, Value};
use zeroship_auth::audit::{self, AuditEvent};

use crate::authz_guard::AuthzGuard;
use crate::AppState;

pub async fn emit_guard_event(
    state: &AppState,
    guard: &AuthzGuard,
    event_type: &'static str,
    client_id: Option<&str>,
    detail: Value,
) -> Result<(), web::HttpResponse> {
    let ev = AuditEvent {
        event_type,
        outcome: "success",
        user_id: Some(&guard.principal_id),
        client_id,
        ip: guard.request_ip,
        auth_method: Some(auth_method(guard)),
        detail,
        ..Default::default()
    };

    audit::emit_strict(&state.auth_pg, &ev).await.map_err(|err| {
        tracing::error!(
            error = %err,
            event_type,
            "control: security audit insert failed"
        );
        web::HttpResponse::InternalServerError().json(&json!({"error": "audit_insert_failed"}))
    })
}

fn auth_method(guard: &AuthzGuard) -> &'static str {
    if guard.token_id.is_some() {
        "pat"
    } else if guard.token_policy.is_some() {
        "oauth"
    } else {
        "console_session"
    }
}
