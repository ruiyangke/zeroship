//! OIDC Core §5.3 UserInfo endpoint.

use std::sync::Arc;

use compio_postgres::Client;
use ntex::http::header::AUTHORIZATION;
use ntex::http::StatusCode;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::oidc::claims::{scope_gated_identity_claims, ScopeGatedIdentityClaims};
use crate::oidc::Issuer;
use crate::store::users;
use zeroship_core::wrapper_revocation;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/userinfo")
            .route(web::get().to(userinfo))
            .route(web::post().to(userinfo)),
    );
}

#[derive(Debug, Serialize)]
struct UserInfoResponse {
    sub: String,
    #[serde(flatten)]
    identity: ScopeGatedIdentityClaims,
}

#[derive(Debug)]
enum UserInfoError {
    /// No usable `Authorization: Bearer` credential was presented at all —
    /// RFC 6750 §3.1 wants a bare `Bearer` challenge (no `error`).
    MissingToken,
    /// A credential was presented but failed verification, or resolved to no
    /// live user — `Bearer error="invalid_token"`.
    InvalidToken,
    /// The access token is valid but was not issued with the `openid` scope,
    /// so it may not be used at the UserInfo endpoint (OIDC Core §5.3).
    InsufficientScope,
    Server,
}

#[allow(clippy::future_not_send)]
async fn userinfo(
    req: HttpRequest,
    db: web::types::State<Arc<Client>>,
    issuer: web::types::State<Arc<Issuer>>,
) -> HttpResponse {
    match userinfo_inner(&req, db.as_ref(), issuer.as_ref()).await {
        Ok(body) => HttpResponse::Ok()
            .content_type("application/json")
            .header("cache-control", "no-store")
            .json(&body),
        Err(UserInfoError::MissingToken) => challenge_response(StatusCode::UNAUTHORIZED, "Bearer"),
        Err(UserInfoError::InvalidToken) => {
            challenge_response(StatusCode::UNAUTHORIZED, r#"Bearer error="invalid_token""#)
        }
        Err(UserInfoError::InsufficientScope) => challenge_response(
            StatusCode::FORBIDDEN,
            r#"Bearer error="insufficient_scope", scope="openid""#,
        ),
        Err(UserInfoError::Server) => HttpResponse::InternalServerError()
            .content_type("application/json")
            .header("cache-control", "no-store")
            .json(&json!({ "error": "server_error" })),
    }
}

#[allow(clippy::future_not_send)]
async fn userinfo_inner(
    req: &HttpRequest,
    db: &Client,
    issuer: &Issuer,
) -> Result<UserInfoResponse, UserInfoError> {
    // Distinguish "no credential" (bare challenge) from "bad credential".
    let token = match bearer_token(req) {
        Some(BearerToken::Present(token)) => token,
        Some(BearerToken::Malformed) => return Err(UserInfoError::InvalidToken),
        None => return Err(UserInfoError::MissingToken),
    };
    let claims = issuer
        .verify_access_token(token)
        .map_err(|_| UserInfoError::InvalidToken)?;
    let revoked =
        wrapper_revocation::is_family_revoked_since(db, &claims.client_id, &claims.sub, claims.iat)
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    client_id = %claims.client_id,
                    sub = %claims.sub,
                    "userinfo: access-token revocation lookup failed"
                );
                UserInfoError::Server
            })?;
    if revoked {
        tracing::debug!(
            client_id = %claims.client_id,
            sub = %claims.sub,
            "userinfo: access-token family is revoked"
        );
        return Err(UserInfoError::InvalidToken);
    }

    // OIDC Core §5.3: the access token MUST have been issued with the `openid`
    // scope. Without this gate a plain resource token (e.g. one minted for an
    // `app:<id>` audience with no `openid`) would be an identity oracle.
    if !claims.scope.split_ascii_whitespace().any(|s| s == "openid") {
        return Err(UserInfoError::InsufficientScope);
    }

    let Some(global_user_id) = global_user_id_for_pairwise_sub(db, &claims.sub).await? else {
        return Err(UserInfoError::InvalidToken);
    };
    let user_id = global_user_id.to_string();
    let Some(user) = users::find_by_id(db, &user_id).await.map_err(|err| {
        tracing::error!(error = %err, "userinfo: user lookup failed");
        UserInfoError::Server
    })? else {
        tracing::debug!(user_id = %user_id, "userinfo: token subject has no user row");
        return Err(UserInfoError::InvalidToken);
    };
    // A disabled/terminated account must not keep leaking identity through a
    // still-live access token (MED-2).
    if user.disabled_at.is_some() {
        tracing::debug!(user_id = %user_id, "userinfo: token subject is disabled");
        return Err(UserInfoError::InvalidToken);
    }

    Ok(UserInfoResponse {
        sub: claims.sub,
        identity: scope_gated_identity_claims(&user, claims.scope.split_ascii_whitespace()),
    })
}

#[allow(clippy::future_not_send)]
async fn global_user_id_for_pairwise_sub(
    db: &Client,
    pairwise_sub: &str,
) -> Result<Option<Uuid>, UserInfoError> {
    let rows = db
        .query(
            "SELECT global_user_id \
             FROM zeroship.app_user_identities \
             WHERE pairwise_sub = $1 AND revoked_at IS NULL \
             LIMIT 1",
            &[&pairwise_sub],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "userinfo: pairwise reverse-map lookup failed");
            UserInfoError::Server
        })?;
    rows.first()
        .map(|row| {
            row.try_get::<_, Uuid>("global_user_id").map_err(|err| {
                tracing::error!(error = %err, "userinfo: reverse-map row decode failed");
                UserInfoError::Server
            })
        })
        .transpose()
}

/// The result of parsing the `Authorization` header: `None` = no Bearer
/// credential offered at all; `Malformed` = a Bearer header we can't parse.
enum BearerToken<'a> {
    Present(&'a str),
    Malformed,
}

fn bearer_token(req: &HttpRequest) -> Option<BearerToken<'_>> {
    let raw = req.headers().get(AUTHORIZATION)?.to_str().ok()?.trim();
    let mut parts = raw.split_ascii_whitespace();
    let Some(scheme) = parts.next() else {
        return None;
    };
    if !scheme.eq_ignore_ascii_case("Bearer") {
        // A different auth scheme is not a Bearer credential for us.
        return None;
    }
    match (parts.next(), parts.next()) {
        (Some(token), None) if !token.is_empty() => Some(BearerToken::Present(token)),
        _ => Some(BearerToken::Malformed),
    }
}

fn challenge_response(status: StatusCode, www_authenticate: &str) -> HttpResponse {
    HttpResponse::build(status)
        .header("www-authenticate", www_authenticate)
        .header("cache-control", "no-store")
        .finish()
}
