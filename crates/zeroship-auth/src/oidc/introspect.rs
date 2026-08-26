//! OAuth 2.0 Token Introspection endpoint (RFC 7662).

use std::sync::Arc;

use compio_postgres::Client;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::AuthConfig;
use crate::oidc::authorization_code::{load_client, OAuthClient, OAuthError};
use crate::oidc::refresh::{
    authenticate_client, authenticated_client_id, client_auth_from_request,
    introspect_refresh_token, ClientAuthMethod, RefreshTokenKeys,
};
use crate::oidc::{AccessTokenClaims, Issuer};
use zeroship_authz::wrapper_revocation;

const ACCESS_PRINCIPAL_ACTIVE_SQL: &str =
    "SELECT 1 \
     FROM zeroship.users owner \
     WHERE owner.disabled_at IS NULL \
       AND owner.anonymized_at IS NULL \
       AND owner.deletion_requested_at IS NULL \
       AND owner.deletion_scheduled_for IS NULL \
       AND (owner.id::text = $1 OR EXISTS ( \
           SELECT 1 FROM zeroship.app_user_identities aui \
           WHERE aui.global_user_id = owner.id \
             AND aui.pairwise_sub = $1 \
             AND aui.revoked_at IS NULL \
       )) \
     LIMIT 1";

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/introspect").route(web::post().to(introspect_post)));
}

#[derive(Debug, Deserialize)]
pub struct IntrospectRequest {
    pub token: Option<String>,
    pub token_type_hint: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

#[allow(clippy::future_not_send)]
pub async fn introspect_post(
    req: HttpRequest,
    form: web::types::Form<IntrospectRequest>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<Client>>,
    issuer: web::types::State<Arc<Issuer>>,
) -> HttpResponse {
    match introspect_inner(&req, form.into_inner(), cfg.as_ref(), db.as_ref(), issuer.as_ref())
        .await
    {
        Ok(body) => HttpResponse::Ok()
            .header("cache-control", "no-store")
            .header("pragma", "no-cache")
            .json(&body),
        Err(err) => super::authorization_code::client_auth_error_response(err),
    }
}

#[allow(clippy::future_not_send)]
async fn introspect_inner(
    req: &HttpRequest,
    form: IntrospectRequest,
    cfg: &AuthConfig,
    db: &Client,
    issuer: &Issuer,
) -> Result<Value, OAuthError> {
    let client_auth =
        client_auth_from_request(req, form.client_id.as_deref(), form.client_secret.as_deref());
    if client_auth.method == ClientAuthMethod::None {
        return Err(OAuthError::invalid_client("client authentication required"));
    }
    let client_id = authenticated_client_id(db, form.client_id.as_deref(), &client_auth).await?;
    let client = load_client(db, &client_id).await?;
    authenticate_client(issuer, &client, &client_auth)?;
    let Some(raw_token) = form.token.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
        return Err(OAuthError::invalid_request("token is required"));
    };

    let hint = form.token_type_hint.as_deref().map(str::trim);
    let active = match hint {
        Some("refresh_token") => {
            if let Some(active) = introspect_refresh(db, cfg, issuer, &client, raw_token).await? {
                Some(active)
            } else {
                introspect_access(db, issuer, &client, raw_token).await?
            }
        }
        Some("access_token") => {
            if let Some(active) = introspect_access(db, issuer, &client, raw_token).await? {
                Some(active)
            } else {
                introspect_refresh(db, cfg, issuer, &client, raw_token).await?
            }
        }
        _ => {
            if let Some(active) = introspect_access(db, issuer, &client, raw_token).await? {
                Some(active)
            } else {
                introspect_refresh(db, cfg, issuer, &client, raw_token).await?
            }
        }
    };
    Ok(active.unwrap_or_else(|| json!({ "active": false })))
}

#[allow(clippy::future_not_send)]
async fn introspect_access(
    db: &Client,
    issuer: &Issuer,
    client: &OAuthClient,
    raw_token: &str,
) -> Result<Option<Value>, OAuthError> {
    let claims = match issuer.verify_access_token(raw_token) {
        Ok(claims) => claims,
        Err(_) => return Ok(None),
    };
    if claims.client_id != client.client_id {
        return Ok(None);
    }
    let revoked =
        wrapper_revocation::is_family_revoked_since(db, &claims.client_id, &claims.sub, claims.iat)
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    client_id = %claims.client_id,
                    sub = %claims.sub,
                    "introspection access-token revocation lookup failed"
                );
                OAuthError::server_error("token revocation store unavailable")
            })?;
    if revoked {
        return Ok(Some(json!({ "active": false })));
    }
    let rows = db
        .query(ACCESS_PRINCIPAL_ACTIVE_SQL, &[&claims.sub])
        .await
        .map_err(|err| {
            tracing::error!(
                error = %err,
                sub = %claims.sub,
                "introspection access-token lifecycle lookup failed"
            );
            OAuthError::server_error("principal lifecycle unavailable")
        })?;
    if rows.is_empty() {
        return Ok(Some(json!({ "active": false })));
    }
    Ok(Some(access_response(claims)))
}

#[allow(clippy::future_not_send)]
async fn introspect_refresh(
    db: &Client,
    cfg: &AuthConfig,
    issuer: &Issuer,
    client: &OAuthClient,
    raw_token: &str,
) -> Result<Option<Value>, OAuthError> {
    let keys = RefreshTokenKeys::from_config(cfg)?;
    Ok(introspect_refresh_token(db, &keys, client, raw_token)
        .await?
        .map(|active| {
            json!({
                "active": true,
                "scope": active.scope,
                "client_id": active.client_id,
                "token_type": active.token_type,
                "exp": active.exp,
                "iat": active.iat,
                "sub": active.sub,
                "aud": active.aud,
                "iss": issuer.issuer(),
            })
        }))
}

fn access_response(claims: AccessTokenClaims) -> Value {
    json!({
        "active": true,
        "scope": claims.scope,
        "client_id": claims.client_id,
        "token_type": "access_token",
        "exp": claims.exp,
        "iat": claims.iat,
        "sub": claims.sub,
        "aud": claims.aud,
        "iss": claims.iss,
    })
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn access_introspection_uses_hard_lifecycle_without_soft_lockout() {
        for column in [
            "disabled_at",
            "anonymized_at",
            "deletion_requested_at",
            "deletion_scheduled_for",
        ] {
            assert!(
                ACCESS_PRINCIPAL_ACTIVE_SQL.contains(&format!("{column} IS NULL")),
                "missing active lifecycle predicate for {column}"
            );
        }
        assert!(!ACCESS_PRINCIPAL_ACTIVE_SQL.contains("locked_until"));
    }
}
