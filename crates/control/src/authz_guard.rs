use std::net::IpAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub request_id: String,
}

impl FromRequest<web::DefaultError> for AuthzGuard {
    type Error = web::Error;

    async fn from_request(req: &HttpRequest, _: &mut Payload) -> Result<Self, Self::Error> {
        let state = req
            .app_state::<Arc<AppState>>()
            .ok_or_else(|| web::error::ErrorInternalServerError("missing app state"))?;
        let request_ip = http_util::source_ip(req, state.trust_proxy)
            .and_then(|ip| ip.parse::<IpAddr>().ok());
        let request_id = request_id(req);

        // Bearer is the ONLY principal path. The console now authenticates to
        // the control plane with a server-only control PAT (or an OAuth bearer)
        // through `@zeroship/control`; the bespoke OIDC-RP console-session path
        // was removed in the R5 cutover (control is a pure API resource server).
        // No bearer ⇒ unauthenticated.
        match guard_from_bearer(req, state, request_ip, request_id).await? {
            Some(guard) => Ok(guard),
            None => Err(web::error::ErrorUnauthorized("unauthenticated").into()),
        }
    }
}

impl AuthzGuard {
    pub async fn require(
        &self,
        action: Action,
        resource: Resource,
        state: &AppState,
    ) -> Result<(), HttpResponse> {
        if let Err(message) = resource.validate_ids() {
            return Err(HttpResponse::BadRequest().json(&json!({
                "error": "invalid_resource_id",
                "message": message,
            })));
        }
        let now = match now_unix() {
            Ok(now) => now,
            Err(err) => {
                tracing::error!(error = %err, "control: authz clock failed");
                return Err(HttpResponse::InternalServerError().json(&json!({
                    "error": "authz_error",
                })));
            }
        };

        let ctx = AuthzContext {
            principal_id: self.principal_id,
            token_id: self.token_id,
            token_policy: self.token_policy.clone(),
            action,
            resource,
            now,
            request_ip: self.request_ip,
            mfa_verified: self.mfa_verified,
            mfa_age_seconds: self.mfa_age_seconds,
            request_id: Some(self.request_id.as_str()),
        };

        match authz::enforce(&state.control_pg, &state.static_policies, &ctx).await {
            Ok(AuthzDecision::Allow) => Ok(()),
            Ok(AuthzDecision::Deny) => Err(
                HttpResponse::Forbidden().json(&json!({"error": "forbidden"})),
            ),
            Err(err) => Err(HttpResponse::InternalServerError()
                .json(&json!({"error": "authz_error", "detail": err.to_string()}))),
        }
    }

    /// Whether this caller holds `action` on `Resource::Any` — the OPERATOR
    /// (fleet-wide) probe, TOKEN-AWARE (it runs through `require`, so a narrowed
    /// PAT that lost the grant returns `false`). Used by the creator-keyed
    /// billing reads to decide whether the caller may target ANOTHER creator via
    /// `?creator_id`. A plain 403 is "not operator"; any other status is an
    /// infrastructure failure propagated as `Err(HttpResponse)` (fail closed).
    pub async fn is_operator(
        &self,
        action: Action,
        state: &AppState,
    ) -> Result<bool, HttpResponse> {
        match self.require(action, Resource::Any, state).await {
            Ok(()) => Ok(true),
            Err(resp) => {
                if resp.status() == ntex::http::StatusCode::FORBIDDEN {
                    Ok(false)
                } else {
                    Err(resp)
                }
            }
        }
    }

    /// Whether this caller can perform `action` ANYWHERE they control: operator
    /// (`Resource::Any`) OR owner/member of at least one app carrying the grant.
    /// The self-scope gate for the creator-keyed billing reads (the caller
    /// reading their OWN creator data must be a billing-capable creator, not
    /// merely any authenticated token). Mirrors `token_handlers`' `Resource::Any`
    /// handling via [`authz::is_authorized_anywhere`].
    pub async fn can_act_anywhere(
        &self,
        action: Action,
        state: &AppState,
    ) -> Result<bool, HttpResponse> {
        let now = match now_unix() {
            Ok(now) => now,
            Err(err) => {
                tracing::error!(error = %err, "control: authz clock failed");
                return Err(HttpResponse::InternalServerError()
                    .json(&json!({"error": "authz_error"})));
            }
        };
        let ctx = AuthzContext {
            principal_id: self.principal_id,
            token_id: self.token_id,
            token_policy: self.token_policy.clone(),
            action,
            resource: Resource::Any,
            now,
            request_ip: self.request_ip,
            mfa_verified: self.mfa_verified,
            mfa_age_seconds: self.mfa_age_seconds,
            request_id: Some(self.request_id.as_str()),
        };
        authz::is_authorized_anywhere(&state.control_pg, &state.static_policies, &ctx)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "control: is_authorized_anywhere failed");
                HttpResponse::InternalServerError()
                    .json(&json!({"error": "authz_error", "detail": err.to_string()}))
            })
    }
}

async fn guard_from_bearer(
    req: &HttpRequest,
    state: &AppState,
    request_ip: Option<IpAddr>,
    request_id: String,
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
            return oauth_guard_from_bearer(raw, state, request_ip, request_id).await;
        }
    };
    let token_id = Uuid::parse_str(&claims.jti)
        .map_err(|_| web::error::ErrorUnauthorized("invalid bearer token id"))?;
    let owner_id = Uuid::parse_str(&claims.owner)
        .map_err(|_| web::error::ErrorUnauthorized("invalid bearer owner"))?;

    let rows = state
        .control_pg
        .query(
            "SELECT owner_id FROM zeroship.permission_tokens \
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
        .control_pg
        .execute(
            "UPDATE zeroship.permission_tokens SET last_used_at = NOW() WHERE id = $1",
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
        request_id,
    }))
}

fn request_id(req: &HttpRequest) -> String {
    req.headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

fn now_unix() -> Result<i64, String> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| format!("clock: {err}"))?
            .as_secs(),
    )
    .map_err(|err| format!("clock overflow: {err}"))
}

async fn oauth_guard_from_bearer(
    token: &str,
    state: &AppState,
    request_ip: Option<IpAddr>,
    request_id: String,
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
        return Err(unauthorized_json("inactive_token"));
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
        request_id,
    }))
}

fn unauthorized_json(error: &'static str) -> web::Error {
    web::error::InternalError::from_response(
        error,
        HttpResponse::Unauthorized().json(&json!({ "error": error })),
    )
    .into()
}
