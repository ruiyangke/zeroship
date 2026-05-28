//! User-facing connected-app OAuth grant handlers.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ntex::web;
use ntex::web::types::{Path, State};
use serde::Serialize;
use serde_json::json;
use zeroship_authz::{Action, Resource};

use crate::authz_guard::AuthzGuard;
use crate::AppState;

#[derive(Debug, Serialize)]
pub struct OauthGrantSummary {
    pub client_id: String,
    pub client_name: String,
    pub client_uri: Option<String>,
    pub logo_uri: Option<String>,
    pub granted_scopes: Vec<String>,
    pub granted_at: String,
    pub last_used_at: Option<String>,
}

/// GET /me/oauth-grants - list active grants for the authenticated user.
pub async fn list_grants(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Err(resp) = authz
        .require(Action::AccountRead, Resource::Any, &state)
        .await
    {
        return resp;
    }

    let rows = match state
        .auth_pg
        .query(
            "SELECT g.client_id, c.client_name, c.client_uri, c.logo_uri, \
                    g.granted_scopes, g.granted_at, g.last_used_at \
             FROM control.oauth_grants g \
             JOIN control.oauth_clients c ON c.client_id = g.client_id \
             WHERE g.user_id = $1 \
             ORDER BY g.granted_at DESC",
            &[&authz.principal_id],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, "control: oauth grant list failed");
            return web::HttpResponse::InternalServerError()
                .json(&json!({"error": "oauth_grant_list_failed"}));
        }
    };

    let grants = rows
        .into_iter()
        .map(|row| {
            let granted_at: DateTime<Utc> = row.get("granted_at");
            let last_used_at: Option<DateTime<Utc>> = row.get("last_used_at");
            OauthGrantSummary {
                client_id: row.get("client_id"),
                client_name: row.get("client_name"),
                client_uri: row.get("client_uri"),
                logo_uri: row.get("logo_uri"),
                granted_scopes: row.get("granted_scopes"),
                granted_at: granted_at.to_rfc3339(),
                last_used_at: last_used_at.map(|value| value.to_rfc3339()),
            }
        })
        .collect::<Vec<_>>();

    web::HttpResponse::Ok().json(&grants)
}

/// DELETE /me/oauth-grants/{client_id} - revoke grant and active tokens.
#[allow(clippy::future_not_send)]
pub async fn revoke_grant(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    client_id: Path<String>,
) -> web::HttpResponse {
    if let Err(resp) = authz
        .require(Action::AccountWrite, Resource::Any, &state)
        .await
    {
        return resp;
    }

    let client_id = client_id.into_inner();
    let deleted = match state
        .auth_pg
        .execute(
            "DELETE FROM control.oauth_grants WHERE user_id = $1 AND client_id = $2",
            &[&authz.principal_id, &client_id],
        )
        .await
    {
        Ok(deleted) => deleted,
        Err(err) => {
            tracing::error!(
                error = %err,
                client_id = %client_id,
                "control: oauth grant delete failed"
            );
            return web::HttpResponse::InternalServerError()
                .json(&json!({"error": "oauth_grant_delete_failed"}));
        }
    };

    if deleted == 0 {
        return web::HttpResponse::NotFound().json(&json!({"error": "oauth_grant_not_found"}));
    }

    if let Err(err) = hydra_revoke_consent_sessions(
        &state.hydra_admin_url,
        &authz.principal_id.to_string(),
        &client_id,
    )
    .await
    {
        tracing::error!(
            error = %err,
            client_id = %client_id,
            subject = %authz.principal_id,
            "control: hydra oauth grant token revoke failed"
        );
        return web::HttpResponse::InternalServerError()
            .json(&json!({"error": "hydra_oauth_grant_revoke_failed"}));
    }

    web::HttpResponse::NoContent().finish()
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/me/oauth-grants").route(web::get().to(list_grants)))
        .service(
            web::resource("/me/oauth-grants/{client_id}").route(web::delete().to(revoke_grant)),
        );
}

#[allow(clippy::future_not_send)]
async fn hydra_revoke_consent_sessions(
    base_url: &str,
    subject: &str,
    client_id: &str,
) -> Result<(), HydraRevokeError> {
    let url = hydra_revoke_consent_url(base_url, subject, client_id);
    let res = cyper::Client::new()
        .request(http::Method::DELETE, url)
        .map_err(|err| HydraRevokeError::Build(err.to_string()))?
        .send()
        .await
        .map_err(|err| HydraRevokeError::Transport(err.to_string()))?;
    let status = res.status().as_u16();
    let body = res
        .text()
        .await
        .map_err(|err| HydraRevokeError::Transport(err.to_string()))?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(HydraRevokeError::Response { status, body })
    }
}

fn hydra_revoke_consent_url(base_url: &str, subject: &str, client_id: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("subject", subject)
        .append_pair("client", client_id)
        .finish();
    format!(
        "{}/admin/oauth2/auth/sessions/consent?{}",
        base_url.trim_end_matches('/'),
        query
    )
}

#[derive(Debug)]
enum HydraRevokeError {
    Build(String),
    Transport(String),
    Response { status: u16, body: String },
}

impl std::fmt::Display for HydraRevokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Build(err) => write!(f, "build request: {err}"),
            Self::Transport(err) => write!(f, "transport: {err}"),
            Self::Response { status, body } => write!(f, "hydra returned {status}: {body}"),
        }
    }
}
