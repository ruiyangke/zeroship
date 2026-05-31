//! User-facing connected-app OAuth grant handlers.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ntex::web;
use ntex::web::types::{Path, State};
use serde::Serialize;
use serde_json::json;
use zeroship_authz::{Action, EntityCache, Resource};

use crate::auth_audit;
use crate::authz_guard::AuthzGuard;
use crate::AppState;

const MAX_CLIENT_ID_PATH_BYTES: usize = 128;

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
             FROM zeroship.oauth_grants g \
             JOIN zeroship.oauth_clients c ON c.client_id = g.client_id \
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
    if !valid_client_id_path_segment(&client_id) {
        return web::HttpResponse::BadRequest().json(&json!({"error": "invalid_client_id"}));
    }

    // Atomic revocation cascade (relay sub-spec §6, B4): DELETE the grant AND
    // revoke the relay alias for THIS (app, user) in ONE transaction on a
    // DEDICATED owned connection — NOT the shared `auth_pg`. Driving a
    // multi-statement transaction on `auth_pg` (the `Arc<Client>` every other
    // control handler pipelines onto) would let a bystander handler's statement
    // interleave INSIDE this BEGIN…COMMIT window — a cross-request corruption
    // hazard. The dedicated client (opened on `auth_db_url`) gives us the
    // `&mut self` the RAII `transaction()` needs and isolates the transaction's
    // snapshot/locks/abort-state from everyone else. After commit, inbound to
    // that alias bounces (5b `revoked_at IS NULL` gate). No window exists where
    // the grant is gone but the alias still forwards.
    let deleted = match crate::relay_revoke::revoke_grant_cascade(
        &state.auth_db_url,
        &authz.principal_id,
        &client_id,
        &state.pairwise_salt,
    )
    .await
    {
        Ok(deleted) => deleted,
        Err(err) => {
            tracing::error!(
                error = %err,
                client_id = %client_id,
                "control: oauth grant revoke cascade failed"
            );
            return web::HttpResponse::InternalServerError()
                .json(&json!({"error": "oauth_grant_delete_failed"}));
        }
    };

    if deleted == 0 {
        return web::HttpResponse::NotFound().json(&json!({"error": "oauth_grant_not_found"}));
    }
    EntityCache::invalidate(authz.principal_id);

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

    if let Err(resp) = auth_audit::emit_guard_event(
        &state,
        &authz,
        "oauth_grant_revoke",
        Some(&client_id),
        json!({
            "client_id": &client_id,
        }),
    )
    .await
    {
        return resp;
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

fn valid_client_id_path_segment(client_id: &str) -> bool {
    !client_id.is_empty() && client_id.len() <= MAX_CLIENT_ID_PATH_BYTES
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_path_segment_is_bounded() {
        assert!(valid_client_id_path_segment("client_123"));
        assert!(valid_client_id_path_segment(&"a".repeat(128)));
        assert!(!valid_client_id_path_segment(""));
        assert!(!valid_client_id_path_segment(&"a".repeat(129)));
    }
}
