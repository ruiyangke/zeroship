//! OIDC Core §5.3 UserInfo endpoint.

use std::sync::Arc;

use compio_postgres::Client;
use ntex::http::header::AUTHORIZATION;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::oidc::claims::{scope_gated_identity_claims, ScopeGatedIdentityClaims};
use crate::oidc::Issuer;
use crate::store::users;

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
    InvalidToken,
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
        Err(UserInfoError::InvalidToken) => invalid_token_response(),
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
    let claims = {
        let token = bearer_token(req).ok_or(UserInfoError::InvalidToken)?;
        issuer
            .verify_access_token(token)
            .map_err(|_| UserInfoError::InvalidToken)?
    };

    let Some(global_user_id) = global_user_id_for_pairwise_sub(db, &claims.sub).await? else {
        return Err(UserInfoError::InvalidToken);
    };
    let user_id = global_user_id.to_string();
    let Some(user) = users::find_by_id(db, &user_id).await.map_err(|err| {
        tracing::error!(error = %err, "userinfo: user lookup failed");
        UserInfoError::Server
    })? else {
        tracing::debug!(user_id = %user_id, "userinfo: token subject has no active user");
        return Err(UserInfoError::InvalidToken);
    };

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
    Ok(rows.first().map(|row| row.get("global_user_id")))
}

fn bearer_token(req: &HttpRequest) -> Option<&str> {
    let raw = req.headers().get(AUTHORIZATION)?.to_str().ok()?.trim();
    let mut parts = raw.split_ascii_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    if !scheme.eq_ignore_ascii_case("Bearer") || token.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(token)
}

fn invalid_token_response() -> HttpResponse {
    HttpResponse::Unauthorized()
        .header("www-authenticate", r#"Bearer error="invalid_token""#)
        .header("cache-control", "no-store")
        .finish()
}
