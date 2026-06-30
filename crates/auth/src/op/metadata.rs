//! JWKS and OAuth/OIDC discovery metadata handlers.

use std::sync::Arc;

use compio_postgres::Client;
use ntex::web::{self, HttpResponse};
use serde_json::{json, Map, Value};

use crate::config::AuthConfig;
use crate::error::{AuthError, Result};

const DISCOVERY_CACHE_CONTROL: &str = "public, max-age=300";
const JWKS_CACHE_CONTROL: &str = "public, max-age=300, stale-while-revalidate=300";
const NO_STORE: &str = "no-store";

#[web::get("/.well-known/jwks.json")]
#[allow(clippy::future_not_send)]
pub async fn jwks(db: web::types::State<Arc<Client>>) -> HttpResponse {
    match jwks_document(db.as_ref()).await {
        Ok(doc) => HttpResponse::Ok()
            .content_type("application/json")
            .header("cache-control", JWKS_CACHE_CONTROL)
            .json(&doc),
        Err(e) => {
            tracing::error!(error = %e, "JWKS unavailable");
            HttpResponse::ServiceUnavailable()
                .content_type("application/json")
                .header("cache-control", NO_STORE)
                .json(&json!({ "error": "jwks_unavailable" }))
        }
    }
}

#[web::get("/.well-known/openid-configuration")]
pub async fn openid_configuration(cfg: web::types::State<Arc<AuthConfig>>) -> HttpResponse {
    discovery_response(cfg.public_url())
}

#[web::get("/.well-known/oauth-authorization-server")]
pub async fn oauth_authorization_server(cfg: web::types::State<Arc<AuthConfig>>) -> HttpResponse {
    discovery_response(cfg.public_url())
}

fn discovery_response(issuer: String) -> HttpResponse {
    let metadata = discovery_metadata(&issuer);
    HttpResponse::Ok()
        .content_type("application/json")
        .header("cache-control", DISCOVERY_CACHE_CONTROL)
        .json(&metadata)
}

/// Build a public JWKS from active/next/retiring registry rows.
pub async fn jwks_document(db: &Client) -> Result<Value> {
    let rows = db
        .query(
            "SELECT kid, public_jwk \
             FROM zeroship.signing_keys \
             WHERE status IN ('active', 'next', 'retiring') \
             ORDER BY CASE status \
                WHEN 'active' THEN 0 \
                WHEN 'next' THEN 1 \
                ELSE 2 \
             END, created_at DESC, kid ASC",
            &[],
        )
        .await
        .map_err(|e| AuthError::Db(format!("select signing_keys JWKS: {e}")))?;

    let mut keys = Vec::with_capacity(rows.len());
    for row in rows {
        let kid: String = row.get("kid");
        let jwk: Value = row.get("public_jwk");
        keys.push(public_jwk_only(&kid, jwk)?);
    }
    Ok(json!({ "keys": keys }))
}

/// Build OIDC Discovery / RFC 8414 metadata. Both well-known endpoints use the
/// same body so issuer/JWKS/endpoint declarations cannot drift.
#[must_use]
pub fn discovery_metadata(issuer: &str) -> Value {
    let issuer = issuer.trim_end_matches('/');
    json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "userinfo_endpoint": format!("{issuer}/userinfo"),
        "end_session_endpoint": format!("{issuer}/logout"),
        "device_authorization_endpoint": format!("{issuer}/device/authorization"),
        "revocation_endpoint": format!("{issuer}/revoke"),
        "jwks_uri": format!("{issuer}/.well-known/jwks.json"),
        "response_types_supported": ["code"],
        "grant_types_supported": [
            "authorization_code",
            "refresh_token",
            "urn:ietf:params:oauth:grant-type:device_code"
        ],
        "scopes_supported": ["openid", "profile", "email", "offline_access"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": [
            "none",
            "client_secret_basic",
            "client_secret_post"
        ],
        "subject_types_supported": ["pairwise"],
        "id_token_signing_alg_values_supported": ["EdDSA"],
        "token_endpoint_auth_signing_alg_values_supported": ["EdDSA"],
        "authorization_signing_alg_values_supported": ["EdDSA"],
        "backchannel_logout_supported": true,
        "backchannel_logout_session_supported": true,
        "frontchannel_logout_supported": false,
        "claims_parameter_supported": false,
        "authorization_response_iss_parameter_supported": true,
        "claims_supported": [
            "iss",
            "sub",
            "aud",
            "exp",
            "iat",
            "jti",
            "client_id",
            "scope",
            "auth_time",
            "nonce",
            "at_hash",
            "email",
            "email_verified",
            "name",
            "picture",
            "amr",
            "acr"
        ]
    })
}

fn public_jwk_only(kid: &str, jwk: Value) -> Result<Value> {
    let obj = jwk.as_object().ok_or_else(|| {
        AuthError::Db(format!("signing_keys.public_jwk for {kid} is not a JSON object"))
    })?;
    let mut public = Map::new();
    for field in ["kty", "crv", "use", "kid", "alg", "x"] {
        if let Some(value) = obj.get(field) {
            public.insert(field.to_string(), value.clone());
        }
    }
    public.insert("kid".to_string(), Value::String(kid.to_string()));
    for required in ["kty", "crv", "kid", "alg", "x"] {
        if !public.contains_key(required) {
            return Err(AuthError::Db(format!(
                "signing_keys.public_jwk for {kid} missing required public field {required}"
            )));
        }
    }
    Ok(Value::Object(public))
}
