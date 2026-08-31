use std::net::IpAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ntex::http::Payload;
use ntex::web::{self, FromRequest, HttpRequest, HttpResponse};
use serde_json::json;
use uuid::Uuid;
use zeroship_authz::{self as authz, Action, AuthzContext, AuthzDecision, Resource};
use zeroship_authn::{AuthnRejection, VerifiedPrincipal};

use crate::{http_util, AppState};

#[derive(Debug)]
pub struct AuthzGuard {
    pub principal_id: Uuid,
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
            .ok_or_else(|| AuthnRejection::internal("missing_app_state"))?;
        let request_ip = http_util::source_ip(req, state.trust_proxy)
            .and_then(|ip| ip.parse::<IpAddr>().ok());
        let request_id = request_id(req);

        // Bearer is the ONLY principal path, and the platform OP is its only
        // issuer. The console authenticates to the control plane with an OAuth
        // access token through `@zeroship/control`; the bespoke OIDC-RP
        // console-session path went in the R5 cutover and the locally-signed
        // personal access token went with the second issuance authority.
        // No bearer means unauthenticated.
        match guard_from_bearer(req, state, request_ip, request_id).await? {
            Some(guard) => Ok(guard),
            None => Err(AuthnRejection::unauthorized("unauthenticated").into()),
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

    /// Whether this caller can perform `action` ANYWHERE they control:
    /// platform-wide (`Resource::Any`) OR owner/member of at least one app
    /// carrying the grant.
    /// The self-scope gate for the creator-keyed billing reads (the caller
    /// reading their OWN creator data must be a billing-capable creator, not
    /// merely any authenticated token), via [`authz::is_authorized_anywhere`].
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
    let mut verified = state
        .bearer_verifier()
        .verify_bearer(raw, request_ip, request_id)
        .await?;

    // Control's first sight of a platform-native creator. Login is an OP-only
    // conversation now, so nothing before this point could have written the
    // principal's grant rows - and until they exist an operator has nothing to
    // delete, which is the whole narrowing mechanism.
    //
    // The request itself was already authorized against the default CLI set,
    // so this is materialization, not authorization, and it runs at most once
    // per principal: the shared materializer is guarded on the
    // `zeroship.identity_links` marker, so the steady state is a pure read.
    if verified.seed_platform_cli_grants {
        match zeroship_authn::platform_cli::materialize_default_grants(
            state.control_pg.as_ref(),
            verified.principal_id,
        )
        .await
        {
            Ok(materialization) if materialization.requires_entitlement_refresh() => {
                verified = state
                    .bearer_verifier()
                    .verify_bearer(raw, request_ip, verified.request_id.clone())
                    .await?;
                if verified.seed_platform_cli_grants {
                    return Err(zeroship_authn::AuthnRejection::internal(
                        "platform_cli_materialization_race",
                    )
                    .into());
                }
            }
            Ok(_) => {}
            Err(err) => {
            // Loud but non-fatal, and the two halves of that are deliberate.
            // Failing the request would lock a creator out over a table they
            // have never heard of, for a request the entitlement rules already
            // permit. But while this keeps failing the marker is never written,
            // so the principal stays on the default-set fallback and an
            // operator's DELETE will not narrow them - which is a silent loss
            // of the capability, hence `error` and not `warn`.
            tracing::error!(
                error = %err,
                principal_id = %verified.principal_id,
                "control: materializing default platform CLI grants failed; \
                 operator narrowing will not take effect for this principal"
            );
            }
        }
    }

    Ok(Some(AuthzGuard::from(verified)))
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

impl From<VerifiedPrincipal> for AuthzGuard {
    fn from(principal: VerifiedPrincipal) -> Self {
        Self {
            principal_id: principal.principal_id,
            token_policy: principal.token_policy,
            mfa_verified: principal.mfa_verified,
            mfa_age_seconds: principal.mfa_age_seconds,
            request_ip: principal.request_ip,
            request_id: principal.request_id,
        }
    }
}
