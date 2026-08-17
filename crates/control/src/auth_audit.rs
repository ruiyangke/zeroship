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
        request_id: Some(guard.request_id.clone()),
        ip: guard.request_ip,
        auth_method: Some(auth_method(guard)),
        detail,
        ..Default::default()
    };

    audit::emit_strict(state.control_pg.as_ref(), &ev).await.map_err(|err| {
        tracing::error!(
            error = %err,
            event_type,
            "control: security audit insert failed"
        );
        web::HttpResponse::InternalServerError().json(&json!({"error": "audit_insert_failed"}))
    })
}

/// The credential class every audited control-plane action was authenticated
/// with. There is exactly one: an OAuth access token the platform OP issued.
/// The console reaches control through `@zeroship/control` with the same
/// bearer; the bespoke OIDC-RP console-session arm went in the R5 cutover and
/// the locally-signed personal access token went with the second issuance
/// authority. `guard` is taken so the signature still names what is being
/// classified if a second class ever returns.
pub fn auth_method(_guard: &AuthzGuard) -> &'static str {
    "oauth"
}
