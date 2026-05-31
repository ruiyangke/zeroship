//! Platform-admin OAuth client registration.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use compio_postgres::error::SqlState;
use ntex::web;
use ntex::web::types::{Json, Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use zeroship_authz::{Action, Resource, Scope};

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
    hydra_client_id: String,
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

#[derive(Debug, Serialize)]
struct HydraCreateClientRequest<'a> {
    client_id: &'a str,
    client_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_uri: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    logo_uri: Option<&'a str>,
    redirect_uris: &'a [String],
    grant_types: &'a [String],
    response_types: &'a [String],
    scope: &'a str,
    token_endpoint_auth_method: &'a str,
    subject_type: &'static str,
    skip_consent: bool,
}

#[derive(Debug, Deserialize)]
struct HydraCreateClientResponse {
    #[allow(dead_code)]
    client_id: String,
    client_secret: Option<String>,
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

    let hydra_body = HydraCreateClientRequest {
        client_id: &body.client_id,
        client_name: &body.client_name,
        client_uri: body.client_uri.as_deref(),
        logo_uri: body.logo_uri.as_deref(),
        redirect_uris: &body.redirect_uris,
        grant_types: &body.grant_types,
        response_types: &body.response_types,
        scope: &body.scope,
        token_endpoint_auth_method: &body.token_endpoint_auth_method,
        subject_type: "public",
        skip_consent,
    };

    let hydra = match hydra_create_client(&state.hydra_admin_url, &hydra_body).await {
        Ok(client) => client,
        Err(err) => return hydra_error_response("create", &body.client_id, err),
    };

    let persisted = insert_oauth_client(
        &state,
        &body,
        &scopes,
        authz.principal_id,
        skip_consent,
    )
    .await;
    let persisted = match persisted {
        Ok(row) => row,
        Err(resp) => {
            match hydra_delete_client(&state.hydra_admin_url, &body.client_id).await {
                Ok(()) => return resp,
                Err(err) => return oauth_rollback_failed_response(&body.client_id, err),
            }
        }
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
            "hydra_client_id": &persisted.hydra_client_id,
        }),
    )
    .await
    {
        return resp;
    }

    web::HttpResponse::Created().json(&CreateOauthClientResponse {
        client: persisted,
        client_secret_show_once: hydra.client_secret.is_some(),
        client_secret: hydra.client_secret,
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
        .auth_pg
        .query(
            "SELECT client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
                    skip_consent, created_at, created_by, hydra_client_id \
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
    if let Err(err) = hydra_delete_client(&state.hydra_admin_url, &client_id).await {
        return hydra_error_response("delete", &client_id, err);
    }

    match state
        .auth_pg
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
        .auth_pg
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
        .auth_pg
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
) -> Result<OauthClientRow, web::HttpResponse> {
    let redirect_uris: Vec<&str> = body.redirect_uris.iter().map(String::as_str).collect();
    let scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let rows = state
        .auth_pg
        .query(
            "INSERT INTO zeroship.oauth_clients \
                (client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
                 skip_consent, created_by, hydra_client_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $1) \
             RETURNING client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
                       skip_consent, created_at, created_by, hydra_client_id",
            &[
                &body.client_id,
                &body.client_name,
                &body.client_uri,
                &body.logo_uri,
                &redirect_uris,
                &scopes,
                &skip_consent,
                &created_by,
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
        hydra_client_id: row.get("hydra_client_id"),
    }
}

#[allow(clippy::future_not_send)]
async fn hydra_create_client(
    base_url: &str,
    body: &HydraCreateClientRequest<'_>,
) -> Result<HydraCreateClientResponse, HydraAdminError> {
    let body_bytes = serde_json::to_vec(body)
        .map_err(|err| HydraAdminError::Encode(err.to_string()))?;
    let res = cyper::Client::new()
        .request(http::Method::POST, hydra_url(base_url, "/admin/clients"))
        .map_err(|err| HydraAdminError::Build(err.to_string()))?
        .header("content-type", "application/json")
        .map_err(|err| HydraAdminError::Build(err.to_string()))?
        .body(body_bytes)
        .send()
        .await
        .map_err(|err| HydraAdminError::Transport(err.to_string()))?;
    finish_hydra_json(res).await
}

#[allow(clippy::future_not_send)]
async fn hydra_delete_client(base_url: &str, client_id: &str) -> Result<(), HydraAdminError> {
    let path = format!("/admin/clients/{}", path_segment(client_id));
    let res = cyper::Client::new()
        .request(http::Method::DELETE, hydra_url(base_url, &path))
        .map_err(|err| HydraAdminError::Build(err.to_string()))?
        .send()
        .await
        .map_err(|err| HydraAdminError::Transport(err.to_string()))?;
    let status = res.status().as_u16();
    let body = res
        .text()
        .await
        .map_err(|err| HydraAdminError::Transport(err.to_string()))?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(HydraAdminError::Response { status, body })
    }
}

async fn finish_hydra_json<T: for<'de> Deserialize<'de>>(
    res: cyper::Response,
) -> Result<T, HydraAdminError> {
    let status = res.status().as_u16();
    let body = res
        .text()
        .await
        .map_err(|err| HydraAdminError::Transport(err.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(HydraAdminError::Response { status, body });
    }
    serde_json::from_str(&body).map_err(|err| HydraAdminError::Decode(format!("{err}: {body}")))
}

fn hydra_url(base_url: &str, path: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), path)
}

fn path_segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[derive(Debug)]
enum HydraAdminError {
    Build(String),
    Encode(String),
    Transport(String),
    Decode(String),
    Response { status: u16, body: String },
}

impl std::fmt::Display for HydraAdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Build(err) => write!(f, "build request: {err}"),
            Self::Encode(err) => write!(f, "encode request: {err}"),
            Self::Transport(err) => write!(f, "transport: {err}"),
            Self::Decode(err) => write!(f, "decode response: {err}"),
            Self::Response { status, body } => write!(f, "hydra returned {status}: {body}"),
        }
    }
}

fn hydra_error_response(
    operation: &str,
    client_id: &str,
    err: HydraAdminError,
) -> web::HttpResponse {
    if matches!(err, HydraAdminError::Response { status: 409, .. }) {
        return web::HttpResponse::Conflict().json(&json!({
            "error": "oauth_client_exists",
            "client_id": client_id,
        }));
    }

    tracing::error!(error = %err, operation, client_id, "control: hydra oauth client operation failed");
    web::HttpResponse::BadGateway().json(&json!({
        "error": "hydra_admin_error",
        "operation": operation,
    }))
}

fn oauth_rollback_failed_response(client_id: &str, err: HydraAdminError) -> web::HttpResponse {
    tracing::error!(
        error = %err,
        client_id,
        "control: oauth client create rollback failed; manual cleanup required"
    );
    web::HttpResponse::BadGateway().json(&json!({
        "error": "oauth_client_cleanup_required",
        "message": "oauth client create failed and cleanup is required",
    }))
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
    use ntex::http::StatusCode;
    use ntex::util::{stream_recv, BytesMut};

    async fn body_json(mut resp: web::HttpResponse) -> serde_json::Value {
        let mut body = resp.take_body();
        let mut buf = BytesMut::new();
        while let Some(item) = stream_recv(&mut body).await {
            buf.extend_from_slice(&item.expect("body chunk"));
        }
        serde_json::from_slice(&buf).expect("body is JSON")
    }

    #[compio::test]
    async fn oauth_rollback_failure_response_is_sanitized_and_actionable() {
        let resp = oauth_rollback_failed_response(
            "oauth-r9",
            HydraAdminError::Response {
                status: 503,
                body: "driver failed: dsn=postgres://internal/path".to_string(),
            },
        );

        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "error": "oauth_client_cleanup_required",
                "message": "oauth client create failed and cleanup is required",
            })
        );
        let rendered = body.to_string();
        assert!(!rendered.contains("driver failed"));
        assert!(!rendered.contains("postgres://internal"));
    }
}
