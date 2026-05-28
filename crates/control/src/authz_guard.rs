use std::net::IpAddr;
use std::sync::Arc;

use ntex::http::Payload;
use ntex::web::{self, FromRequest, HttpRequest, HttpResponse};
use serde_json::json;
use uuid::Uuid;
use zeroship_authz::{self as authz, Action, AuthzContext, AuthzDecision, Resource};

use crate::{http_util, AppState};

#[derive(Debug)]
pub struct AuthzGuard {
    pub principal_id: Uuid,
    pub token_id: Option<Uuid>,
    pub token_policy: Option<authz::Policy>,
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
            token_policy: self.token_policy.clone(),
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
        token_policy: None,
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
    let claims = match state.pat_issuer.verify(raw) {
        Ok(claims) => claims,
        Err(err) => {
            tracing::debug!(
                error = %err,
                "control: bearer was not a valid PAT; trying OAuth introspection"
            );
            return oauth_guard_from_bearer(raw, state, request_ip).await;
        }
    };
    let token_id = Uuid::parse_str(&claims.jti)
        .map_err(|_| web::error::ErrorUnauthorized("invalid bearer token id"))?;
    let owner_id = Uuid::parse_str(&claims.owner)
        .map_err(|_| web::error::ErrorUnauthorized("invalid bearer owner"))?;

    let rows = state
        .auth_pg
        .query(
            "SELECT owner_id FROM control.permission_tokens \
             WHERE id = $1 \
               AND owner_id = $2 \
               AND policy_hash = $3 \
               AND kind = 'pat' \
               AND revoked_at IS NULL \
               AND (expires_at IS NULL OR expires_at > NOW())",
            &[&token_id, &owner_id, &claims.policy_hash],
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

    if let Err(err) = state
        .auth_pg
        .execute(
            "UPDATE control.permission_tokens SET last_used_at = NOW() WHERE id = $1",
            &[&token_id],
        )
        .await
    {
        tracing::warn!(error = %err, "control: permission token last_used_at update failed");
    }

    Ok(Some(AuthzGuard {
        principal_id,
        token_id: Some(token_id),
        token_policy: None,
        mfa_verified: false,
        mfa_age_seconds: None,
        request_ip,
    }))
}

async fn oauth_guard_from_bearer(
    token: &str,
    state: &AppState,
    request_ip: Option<IpAddr>,
) -> Result<Option<AuthzGuard>, web::Error> {
    let result = state
        .hydra_introspector
        .introspect(token)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "control: hydra introspect failed");
            web::error::ErrorUnauthorized("oauth introspection failed")
        })?;
    if !result.active {
        return Err(web::error::ErrorUnauthorized("inactive oauth token").into());
    }
    if !result.aud.as_ref().is_some_and(|audiences| {
        audiences
            .iter()
            .any(|audience| audience == &state.expected_oauth_audience)
    }) {
        return Err(unauthorized_json("wrong_audience"));
    }

    let sub = result
        .sub
        .ok_or_else(|| web::error::ErrorUnauthorized("missing oauth sub"))?;
    let principal_id =
        Uuid::parse_str(&sub).map_err(|_| web::error::ErrorUnauthorized("invalid oauth sub"))?;

    let raw_scope = result.scope.unwrap_or_default();
    let scopes = authz::parse_scope_string(&raw_scope)
        .map_err(|_| web::error::ErrorUnauthorized("invalid oauth scope"))?;
    let token_policy = authz::scopes_to_policy(&scopes);

    Ok(Some(AuthzGuard {
        principal_id,
        token_id: None,
        token_policy: Some(token_policy),
        mfa_verified: false,
        mfa_age_seconds: None,
        request_ip,
    }))
}

fn unauthorized_json(error: &'static str) -> web::Error {
    web::error::InternalError::from_response(
        error,
        HttpResponse::Unauthorized().json(&json!({ "error": error })),
    )
    .into()
}
