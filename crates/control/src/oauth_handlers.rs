//! Platform-admin OAuth client registration.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use compio_postgres::error::SqlState;
use ntex::web;
use ntex::web::types::{Json, Path, State};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use zeroship_authz::{Action, Resource, Scope};
use zeroship_core::auth::hash_api_key;

use crate::auth_audit;
use crate::authz_guard::AuthzGuard;
use crate::AppState;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateOauthClientBody {
    pub client_id: String,
    pub client_name: String,
    pub client_uri: Option<String>,
    pub logo_uri: Option<String>,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub scope: String,
    pub token_endpoint_auth_method: String,
}

#[derive(Debug, Serialize)]
struct OauthClientRow {
    client_id: String,
    client_name: String,
    client_uri: Option<String>,
    logo_uri: Option<String>,
    redirect_uris: Vec<String>,
    scopes: Vec<String>,
    skip_consent: bool,
    created_at: DateTime<Utc>,
    created_by: Option<Uuid>,
}

#[derive(Debug, Serialize)]
struct CreateOauthClientResponse {
    #[serde(flatten)]
    client: OauthClientRow,
    client_secret: Option<String>,
    client_secret_show_once: bool,
}

#[derive(Debug, Serialize)]
struct DeleteOauthClientResponse {
    deleted: bool,
}

pub async fn create_oauth_client(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<CreateOauthClientBody>,
) -> web::HttpResponse {
    if let Err(resp) = authz
        .require(Action::PlatformPoliciesWrite, Resource::Any, &state)
        .await
    {
        return resp;
    }

    let scopes = match validate_scope_list(&body.scope) {
        Ok(scopes) => scopes,
        Err(resp) => return resp,
    };
    if let Err(resp) = validate_create_body(&body) {
        return resp;
    }
    if let Err(resp) = ensure_client_absent(&state, &body.client_id).await {
        return resp;
    }
    let skip_consent = state.is_trusted(&body.client_id);
    let client_secret = if body.token_endpoint_auth_method == "none" {
        None
    } else {
        Some(generate_client_secret())
    };

    let persisted = insert_oauth_client(
        &state,
        &body,
        &scopes,
        authz.principal_id,
        skip_consent,
        client_secret.as_deref(),
    )
    .await;
    let persisted = match persisted {
        Ok(row) => row,
        Err(resp) => return resp,
    };
    if let Err(resp) = auth_audit::emit_guard_event(
        &state,
        &authz,
        "oauth_client_create",
        Some(&persisted.client_id),
        json!({
            "client_id": &persisted.client_id,
            "client_name": &persisted.client_name,
            "redirect_uris": &persisted.redirect_uris,
            "scopes": &persisted.scopes,
            "skip_consent": persisted.skip_consent,
        }),
    )
    .await
    {
        return resp;
    }

    web::HttpResponse::Created().json(&CreateOauthClientResponse {
        client: persisted,
        client_secret_show_once: client_secret.is_some(),
        client_secret,
    })
}

pub async fn list_oauth_clients(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Err(resp) = authz
        .require(Action::PlatformPoliciesWrite, Resource::Any, &state)
        .await
    {
        return resp;
    }

    let rows = match state
        .control_pg
        .query(
            "SELECT client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
                    skip_consent, created_at, created_by \
             FROM zeroship.oauth_clients \
             ORDER BY created_at DESC, client_id ASC",
            &[],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, "control: oauth client list failed");
            return db_error();
        }
    };

    let clients = rows.iter().map(row_to_oauth_client).collect::<Vec<_>>();
    web::HttpResponse::Ok().json(&clients)
}

pub async fn delete_oauth_client(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    id: Path<String>,
) -> web::HttpResponse {
    if let Err(resp) = authz
        .require(Action::PlatformPoliciesWrite, Resource::Any, &state)
        .await
    {
        return resp;
    }

    let client_id = id.into_inner();
    if let Err(resp) = ensure_client_present(&state, &client_id).await {
        return resp;
    }
    match state
        .control_pg
        .execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
    {
        Ok(_) => {
            if let Err(resp) = auth_audit::emit_guard_event(
                &state,
                &authz,
                "oauth_client_delete",
                Some(&client_id),
                json!({
                    "client_id": client_id,
                }),
            )
            .await
            {
                return resp;
            }

            web::HttpResponse::Ok().json(&DeleteOauthClientResponse { deleted: true })
        }
        Err(err) => {
            tracing::error!(error = %err, client_id = %client_id, "control: oauth client delete failed");
            db_error()
        }
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/admin/oauth-clients")
            .route(web::post().to(create_oauth_client))
            .route(web::get().to(list_oauth_clients)),
    )
    .service(
        web::resource("/admin/oauth-clients/{id}")
            .route(web::delete().to(delete_oauth_client)),
    );
}

fn validate_scope_list(raw: &str) -> Result<Vec<String>, web::HttpResponse> {
    let mut scopes = Vec::new();
    for scope in raw.split_whitespace() {
        match Scope::parse(scope) {
            Ok(parsed) => scopes.push(parsed.as_str().to_owned()),
            Err(_) => {
                return Err(web::HttpResponse::BadRequest().json(&json!({
                    "error": "invalid_scope",
                    "scope": scope,
                })));
            }
        }
    }
    if scopes.is_empty() {
        return Err(web::HttpResponse::BadRequest().json(&json!({
            "error": "invalid_scope",
            "message": "scope must contain at least one known scope",
        })));
    }
    Ok(scopes)
}

fn validate_create_body(body: &CreateOauthClientBody) -> Result<(), web::HttpResponse> {
    if body.client_id.trim().is_empty() {
        return Err(bad_request("invalid_client_id", "client_id is required"));
    }
    if body.client_name.trim().is_empty() {
        return Err(bad_request("invalid_client_name", "client_name is required"));
    }
    if body.redirect_uris.is_empty() {
        return Err(bad_request(
            "invalid_redirect_uris",
            "redirect_uris must contain at least one URI",
        ));
    }
    if !matches!(
        body.token_endpoint_auth_method.as_str(),
        "client_secret_basic" | "none"
    ) {
        return Err(bad_request(
            "invalid_token_endpoint_auth_method",
            "token_endpoint_auth_method must be client_secret_basic or none",
        ));
    }
    Ok(())
}

async fn ensure_client_absent(
    state: &AppState,
    client_id: &str,
) -> Result<(), web::HttpResponse> {
    let rows = state
        .control_pg
        .query(
            "SELECT 1 FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, client_id, "control: oauth client duplicate check failed");
            db_error()
        })?;
    if rows.is_empty() {
        Ok(())
    } else {
        Err(web::HttpResponse::Conflict().json(&json!({
            "error": "oauth_client_exists",
            "client_id": client_id,
        })))
    }
}

async fn ensure_client_present(
    state: &AppState,
    client_id: &str,
) -> Result<(), web::HttpResponse> {
    let rows = state
        .control_pg
        .query(
            "SELECT 1 FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, client_id, "control: oauth client lookup failed");
            db_error()
        })?;
    if rows.is_empty() {
        Err(web::HttpResponse::NotFound().json(&json!({
            "error": "oauth_client_not_found",
            "client_id": client_id,
        })))
    } else {
        Ok(())
    }
}

async fn insert_oauth_client(
    state: &AppState,
    body: &CreateOauthClientBody,
    scopes: &[String],
    created_by: Uuid,
    skip_consent: bool,
    client_secret: Option<&str>,
) -> Result<OauthClientRow, web::HttpResponse> {
    let redirect_uris: Vec<&str> = body.redirect_uris.iter().map(String::as_str).collect();
    let scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let client_secret_hash = client_secret.map(hash_api_key);
    let refresh_allowed = body.grant_types.iter().any(|grant| grant == "refresh_token");
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.oauth_clients \
                (client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
                 skip_consent, created_by, client_secret_hash, \
                 refresh_allowed, token_endpoint_auth_method) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             RETURNING client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
                       skip_consent, created_at, created_by",
            &[
                &body.client_id,
                &body.client_name,
                &body.client_uri,
                &body.logo_uri,
                &redirect_uris,
                &scopes,
                &skip_consent,
                &created_by,
                &client_secret_hash,
                &refresh_allowed,
                &body.token_endpoint_auth_method,
            ],
        )
        .await
        .map_err(|err| {
            if err.code() == Some(&SqlState::UNIQUE_VIOLATION) {
                web::HttpResponse::Conflict().json(&json!({
                    "error": "oauth_client_exists",
                    "client_id": body.client_id,
                }))
            } else {
                tracing::error!(error = %err, client_id = %body.client_id, "control: oauth client insert failed");
                db_error()
            }
        })?;

    rows.first()
        .map(row_to_oauth_client)
        .ok_or_else(|| db_error())
}

fn row_to_oauth_client(row: &compio_postgres::Row) -> OauthClientRow {
    OauthClientRow {
        client_id: row.get("client_id"),
        client_name: row.get("client_name"),
        client_uri: row.get("client_uri"),
        logo_uri: row.get("logo_uri"),
        redirect_uris: row.get("redirect_uris"),
        scopes: row.get("scopes"),
        skip_consent: row.get("skip_consent"),
        created_at: row.get("created_at"),
        created_by: row.get("created_by"),
    }
}

fn generate_client_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn bad_request(error: &str, message: &str) -> web::HttpResponse {
    web::HttpResponse::BadRequest().json(&json!({
        "error": error,
        "message": message,
    }))
}

fn db_error() -> web::HttpResponse {
    web::HttpResponse::InternalServerError().json(&json!({"error": "database error"}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_client_secret_shape_is_hex_32_bytes() {
        let secret = generate_client_secret();
        assert_eq!(secret.len(), 64);
        assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
