//! JWKS and OAuth/OIDC discovery metadata handlers.

use std::sync::Arc;

use ntex::web::{self, HttpResponse};
use serde_json::{json, Map, Value};
use zeroship_core::device_grant;
use zeroship_data_orm::Database;

use crate::config::AuthConfig;
use crate::error::{AuthError, Result};

mod registry;

const DISCOVERY_CACHE_CONTROL: &str = "public, max-age=300";
pub(crate) const JWKS_MAX_AGE_SECS: i64 = 5 * 60;
pub(crate) const JWKS_STALE_WHILE_REVALIDATE_SECS: i64 = 5 * 60;
const NO_STORE: &str = "no-store";

fn jwks_cache_control() -> String {
    format!(
        "public, max-age={JWKS_MAX_AGE_SECS}, \
         stale-while-revalidate={JWKS_STALE_WHILE_REVALIDATE_SECS}"
    )
}

#[web::get("/.well-known/jwks.json")]
#[allow(clippy::future_not_send)]
pub async fn jwks(db: web::types::State<Database>) -> HttpResponse {
    match jwks_document(&db).await {
        Ok(doc) => HttpResponse::Ok()
            .content_type("application/json")
            .header("cache-control", jwks_cache_control())
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
    openid_configuration_handler(cfg).await
}

pub async fn openid_configuration_handler(cfg: web::types::State<Arc<AuthConfig>>) -> HttpResponse {
    discovery_response(cfg.op_issuer_url())
}

#[web::get("/.well-known/oauth-authorization-server")]
pub async fn oauth_authorization_server(cfg: web::types::State<Arc<AuthConfig>>) -> HttpResponse {
    oauth_authorization_server_handler(cfg).await
}

pub async fn oauth_authorization_server_handler(
    cfg: web::types::State<Arc<AuthConfig>>,
) -> HttpResponse {
    discovery_response(cfg.op_issuer_url())
}

fn discovery_response(issuer: String) -> HttpResponse {
    let metadata = discovery_metadata(&issuer);
    HttpResponse::Ok()
        .content_type("application/json")
        .header("cache-control", DISCOVERY_CACHE_CONTROL)
        .json(&metadata)
}

/// Build a public JWKS from active/next/retiring registry rows.
///
/// # Errors
/// Returns registry read failures or invalid public key data.
#[allow(clippy::future_not_send, reason = "the ORM belongs to this compio runtime")]
pub async fn jwks_document(db: &Database) -> Result<Value> {
    let rows = registry::published_keys(db).await?;

    let mut keys = Vec::with_capacity(rows.len());
    for row in rows {
        let jwk = zeroship_data_orm::value::from_value(row.public_jwk)
            .map_err(|error| AuthError::Db(format!("decode signing key {}: {error}", row.kid)))?;
        keys.push(public_jwk_only(&row.kid, jwk)?);
    }
    Ok(json!({ "keys": keys }))
}

/// Build OIDC Discovery / RFC 8414 metadata. Both well-known endpoints use the
/// same body so issuer/JWKS/endpoint declarations cannot drift.
#[must_use]
pub fn discovery_metadata(issuer: &str) -> Value {
    let issuer = issuer.trim_end_matches('/');
    // The advertised scope list is DERIVED, not restated. It was a literal
    // array until the organization vocabulary landed: `organization:create`
    // and `organization:read` joined `PLATFORM_CLI_ISSUABLE_SCOPES` so a fresh
    // account's first `zeroship deploy` could mint the organization it needs,
    // this list did not move, and the discovery document went on telling every
    // client the CLI's own scopes were not supported here.
    //
    // The identity scopes are separate because they are not creator authority
    // at all - `IDENTITY_SCOPES` in the consent handler treats them as always
    // self-grantable, and they are namespace (b) rather than (a).
    let scopes_supported: Vec<&str> = ["openid", "profile", "email", "offline_access"]
        .into_iter()
        .chain(device_grant::PLATFORM_CLI_ISSUABLE_SCOPES)
        .collect();
    json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        // The two the CLI drives come from `zeroship_core::device_grant`, the
        // one definition its client also reads. Spelling them twice is how the
        // producer and the consumer drifted before.
        "token_endpoint": format!("{issuer}{}", device_grant::TOKEN_PATH),
        "userinfo_endpoint": format!("{issuer}/userinfo"),
        "end_session_endpoint": format!("{issuer}/logout"),
        "device_authorization_endpoint":
            format!("{issuer}{}", device_grant::DEVICE_AUTHORIZATION_PATH),
        "revocation_endpoint": format!("{issuer}/revoke"),
        "introspection_endpoint": format!("{issuer}/introspect"),
        "jwks_uri": format!("{issuer}/.well-known/jwks.json"),
        "response_types_supported": ["code"],
        "grant_types_supported": [
            "authorization_code",
            "refresh_token",
            "urn:ietf:params:oauth:grant-type:device_code"
        ],
        "scopes_supported": scopes_supported,
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": [
            "none",
            "client_secret_basic",
            "client_secret_post"
        ],
        "subject_types_supported": ["public", "pairwise"],
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

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_core::device_grant::PLATFORM_CLI_ISSUABLE_SCOPES;

    #[test]
    fn discovery_advertises_public_subjects_and_cli_scopes() {
        let metadata = discovery_metadata("https://auth.zeroship.test/oauth2");
        assert_eq!(
            metadata["subject_types_supported"],
            json!(["public", "pairwise"])
        );

        let scopes = metadata["scopes_supported"]
            .as_array()
            .expect("scopes_supported array");
        for scope in PLATFORM_CLI_ISSUABLE_SCOPES {
            assert!(
                scopes.iter().any(|advertised| advertised == scope),
                "missing CLI scope {scope}"
            );
        }
    }

    /// The advertised list is the identity scopes plus the CLI-issuable set and
    /// NOTHING ELSE, so the document cannot advertise an authority the platform
    /// does not issue.
    ///
    /// The test above only checks one direction, which is how this list came to
    /// be short by two: adding a scope to `PLATFORM_CLI_ISSUABLE_SCOPES` made
    /// that test red, but a scope removed from the vocabulary while staying in
    /// a literal array here would have stayed green forever - an OP telling
    /// every client it supports a scope no `Scope::parse` accepts, which is a
    /// refused authorization at the point a creator is already waiting.
    #[test]
    fn discovery_advertises_nothing_beyond_identity_and_the_cli_set() {
        let metadata = discovery_metadata("https://auth.zeroship.test/oauth2");
        let scopes: Vec<String> = metadata["scopes_supported"]
            .as_array()
            .expect("scopes_supported array")
            .iter()
            .map(|value| value.as_str().expect("scope string").to_owned())
            .collect();
        assert!(
            scopes.len() >= 5,
            "ruled on {} advertised scope(s)",
            scopes.len()
        );

        let identity = ["openid", "profile", "email", "offline_access"];
        for scope in &scopes {
            assert!(
                identity.contains(&scope.as_str())
                    || PLATFORM_CLI_ISSUABLE_SCOPES.contains(&scope.as_str()),
                "{scope} is advertised and is neither an identity scope nor one \
                 the platform CLI may be issued"
            );
        }
    }
}
