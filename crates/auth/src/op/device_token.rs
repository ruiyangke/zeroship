//! Internal OP mint endpoint for platform resource-server access tokens.

use std::sync::Arc;

use ntex::web::{self, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use zeroship_authz::Scope;
use zeroship_core::auth::{extract_bearer, validate_control_key};

use crate::config::AuthConfig;
use crate::op::{Issuer, PrincipalAccessTokenMint, ACCESS_TOKEN_TTL_SECS};

pub const INTERNAL_PLATFORM_TOKEN_PATH: &str = "/internal/platform-token";

#[derive(Debug, Deserialize)]
pub struct MintPlatformTokenRequest {
    pub principal_id: String,
    pub audience: String,
    pub client_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub ttl_secs: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct MintPlatformTokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: u64,
    pub scope: String,
    pub provider: &'static str,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource(INTERNAL_PLATFORM_TOKEN_PATH).route(web::post().to(mint_platform_token)),
    );
}

#[allow(clippy::future_not_send)]
pub async fn mint_platform_token(
    req: HttpRequest,
    cfg: web::types::State<Arc<AuthConfig>>,
    issuer: web::types::State<Arc<Issuer>>,
    body: web::types::Json<MintPlatformTokenRequest>,
) -> HttpResponse {
    if !authorized(&req, cfg.as_ref()) {
        return HttpResponse::Unauthorized().json(&json!({"error": "unauthorized"}));
    }

    let principal_id = body.principal_id.trim();
    if Uuid::parse_str(principal_id).is_err() {
        return HttpResponse::BadRequest().json(&json!({"error": "invalid_principal_id"}));
    }
    if body.audience.trim().is_empty() || body.client_id.trim().is_empty() {
        return HttpResponse::BadRequest().json(&json!({"error": "invalid_token_request"}));
    }
    if body
        .scopes
        .iter()
        .any(|scope| Scope::parse(scope.trim()).is_err())
    {
        return HttpResponse::BadRequest().json(&json!({"error": "invalid_scope"}));
    }
    let ttl_secs = body.ttl_secs.unwrap_or(ACCESS_TOKEN_TTL_SECS);
    if ttl_secs <= 0 {
        return HttpResponse::BadRequest().json(&json!({"error": "invalid_ttl"}));
    }

    let scopes: Vec<String> = body
        .scopes
        .iter()
        .map(|scope| scope.trim().to_string())
        .filter(|scope| !scope.is_empty())
        .collect();
    let access_token = match issuer.issue_principal_access_token(&PrincipalAccessTokenMint {
        principal_id,
        audience: body.audience.trim(),
        client_id: body.client_id.trim(),
        scopes: &scopes,
        ttl_secs: Some(ttl_secs),
    }) {
        Ok(token) => token,
        Err(err) => {
            tracing::error!(error = %err, "auth: platform token mint failed");
            return HttpResponse::InternalServerError().json(&json!({"error": "mint_failed"}));
        }
    };

    HttpResponse::Ok().json(&MintPlatformTokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: ttl_secs as u64,
        scope: scopes.join(" "),
        provider: "platform",
    })
}

fn authorized(req: &HttpRequest, cfg: &AuthConfig) -> bool {
    let expected = cfg.control_key.trim();
    if expected.is_empty() {
        return false;
    }
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    extract_bearer(header).is_some_and(|provided| validate_control_key(provided, expected))
}
