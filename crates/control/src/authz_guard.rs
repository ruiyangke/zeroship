use std::net::IpAddr;
use std::sync::Arc;

use base64::Engine as _;
use ntex::http::Payload;
use ntex::web::{self, FromRequest, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;
use zeroship_authz::{self as authz, Action, AuthzContext, AuthzDecision, Resource};

use crate::{http_util, AppState};

#[derive(Debug)]
pub struct AuthzGuard {
    pub principal_id: Uuid,
    pub token_id: Option<Uuid>,
    pub mfa_verified: bool,
    pub mfa_age_seconds: Option<u32>,
    pub request_ip: Option<IpAddr>,
}

impl FromRequest<web::DefaultError> for AuthzGuard {
    type Error = web::Error;

    async fn from_request(req: &HttpRequest, _: &mut Payload) -> Result<Self, Self::Error> {
        let state = req
            .app_state::<Arc<AppState>>()
            .ok_or_else(|| web::error::ErrorInternalServerError("missing app state"))?;
        let request_ip = http_util::source_ip(req, state.trust_proxy)
            .and_then(|ip| ip.parse::<IpAddr>().ok());

        if let Some(guard) = guard_from_bearer(req, state, request_ip).await? {
            return Ok(guard);
        }

        guard_from_session(req, state, request_ip).await
    }
}

impl AuthzGuard {
    pub async fn require(
        &self,
        action: Action,
        resource: Resource,
        state: &AppState,
    ) -> Result<(), HttpResponse> {
        let ctx = AuthzContext {
            principal_id: self.principal_id,
            token_id: self.token_id,
            action,
            resource,
            request_ip: self.request_ip,
            mfa_verified: self.mfa_verified,
            mfa_age_seconds: self.mfa_age_seconds,
            request_id: None,
        };

        match authz::enforce(&state.auth_pg, &state.static_policies, &ctx).await {
            Ok(AuthzDecision::Allow) => Ok(()),
            Ok(AuthzDecision::Deny) => Err(
                HttpResponse::Forbidden().json(&json!({"error": "forbidden"})),
            ),
            Err(err) => Err(HttpResponse::InternalServerError()
                .json(&json!({"error": "authz_error", "detail": err.to_string()}))),
        }
    }
}

async fn guard_from_session(
    req: &HttpRequest,
    state: &AppState,
    request_ip: Option<IpAddr>,
) -> Result<AuthzGuard, web::Error> {
    let session = crate::api::require_console_session(req, state)
        .await
        .map_err(|_| web::error::ErrorUnauthorized("unauthorized"))?;
    let principal_id = Uuid::parse_str(&session.user_id)
        .map_err(|_| web::error::ErrorUnauthorized("invalid session subject"))?;

    Ok(AuthzGuard {
        principal_id,
        token_id: None,
        mfa_verified: false,
        mfa_age_seconds: None,
        request_ip,
    })
}

async fn guard_from_bearer(
    req: &HttpRequest,
    state: &AppState,
    request_ip: Option<IpAddr>,
) -> Result<Option<AuthzGuard>, web::Error> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let Some(raw) = zeroship_core::auth::extract_bearer(header) else {
        return Ok(None);
    };
    let token_id = extract_token_id(raw)
        .ok_or_else(|| web::error::ErrorUnauthorized("invalid bearer token"))?;

    let rows = state
        .auth_pg
        .query(
            "SELECT owner_id FROM control.permission_tokens \
             WHERE id = $1 \
               AND revoked_at IS NULL \
               AND (expires_at IS NULL OR expires_at > NOW())",
            &[&token_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "control: permission token lookup failed");
            web::error::ErrorInternalServerError("permission token lookup failed")
        })?;
    let row = rows
        .first()
        .ok_or_else(|| web::error::ErrorUnauthorized("permission token not active"))?;
    let principal_id: Uuid = row.get("owner_id");

    Ok(Some(AuthzGuard {
        principal_id,
        token_id: Some(token_id),
        mfa_verified: false,
        mfa_age_seconds: None,
        request_ip,
    }))
}

fn extract_token_id(raw: &str) -> Option<Uuid> {
    if let Ok(id) = Uuid::parse_str(raw) {
        return Some(id);
    }

    let claims_segment = raw.split('.').nth(1)?;
    let claims_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(claims_segment)
        .ok()?;
    let claims: BearerClaims = serde_json::from_slice(&claims_bytes).ok()?;
    Uuid::parse_str(&claims.tid).ok()
}

#[derive(Deserialize)]
struct BearerClaims {
    tid: String,
}
