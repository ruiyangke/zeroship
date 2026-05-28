//! Admin API for per-app vars + secret names.
//!
//! Mutations require master key (same auth as the rest of the admin API).
//! Secrets are write-only over this surface — GET returns names and
//! `updated_at` timestamps, never values. Rotation = PUT a fresh value.

use std::sync::Arc;

use ntex::web::{self, types::{Json, Path, State}};
use serde::Deserialize;
use uuid::Uuid;
use zeroship_authz::{Action as AuthzAction, Resource};

use crate::audit::{self, Action, AuditEntry};
use crate::authz_guard::AuthzGuard;
use crate::http_util;
use crate::AppState;
use crate::env_store::EnvError;

pub const ENV_MUTATION_PAYLOAD_BYTES: usize = 80 * 1024;

fn source_ip(req: &web::HttpRequest, state: &AppState) -> Option<String> {
    http_util::source_ip(req, state.trust_proxy)
}

async fn admin_rate_limit(req: &web::HttpRequest, state: &AppState) -> Option<web::HttpResponse> {
    http_util::rate_limit(
        req,
        state.auth_pg.as_ref(),
        "admin",
        state.admin_limiter.quota(),
        state.trust_proxy,
    )
    .await
}

fn bad_uuid() -> web::HttpResponse {
    web::HttpResponse::BadRequest().json(&serde_json::json!({"error": "bad app_id"}))
}

fn env_err_response(e: EnvError) -> web::HttpResponse {
    use EnvError::*;
    match &e {
        BadKey(_) => web::HttpResponse::BadRequest()
            .json(&serde_json::json!({"error": e.to_string()})),
        TooLarge(_) => web::HttpResponse::PayloadTooLarge()
            .json(&serde_json::json!({"error": e.to_string()})),
        AppNotFound => web::HttpResponse::NotFound()
            .json(&serde_json::json!({"error":"app not found"})),
        // Db / Crypto messages may contain internal details (SQLSTATEs,
        // column names, crypto internals). Log the raw error to stderr
        // but return a generic body to the client.
        Db(_) | Crypto(_) | MasterKeyRequired => {
            tracing::error!(error = %e, "control: env_store error");
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error":"internal error"}))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SetKv {
    pub key: String,
    pub value: String,
}

// ------------------------------------------------------------------
// Vars
// ------------------------------------------------------------------

pub async fn list_vars(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await { return r; }
    let Ok(id) = Uuid::parse_str(&path) else { return bad_uuid(); };
    if let Err(resp) = authz
        .require(AuthzAction::EnvRead, Resource::App { id: id.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.env_store.list_vars(id).await {
        Ok(rows) => {
            let items: Vec<_> = rows
                .into_iter()
                .map(|(k, v)| serde_json::json!({"key": k, "value": v}))
                .collect();
            web::HttpResponse::Ok().json(&serde_json::json!({"vars": items}))
        }
        Err(e) => env_err_response(e),
    }
}

pub async fn set_var(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<SetKv>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await { return r; }
    let Ok(id) = Uuid::parse_str(&path) else { return bad_uuid(); };
    if let Err(resp) = authz
        .require(AuthzAction::EnvWrite, Resource::App { id: id.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.env_store.set_var(id, &body.key, &body.value).await {
        Ok(()) => {
            let ip = source_ip(&req, &state);
            audit::log(&state.registry, AuditEntry {
                app_id: Some(id),
                creator_id: None,
                actor_user_id: Some(authz.principal_id),
                actor_token_id: authz.token_id,
                action: Action::SetVar,
                resource: Some(&body.key),
                source_ip: ip.as_deref(),
            }).await;
            web::HttpResponse::NoContent().finish()
        }
        Err(e) => env_err_response(e),
    }
}

pub async fn delete_var(
    req: web::HttpRequest,
    path: Path<(String, String)>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await { return r; }
    let (id_s, key) = path.into_inner();
    let Ok(id) = Uuid::parse_str(&id_s) else { return bad_uuid(); };
    if let Err(resp) = authz
        .require(AuthzAction::EnvWrite, Resource::App { id: id.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.env_store.delete_var(id, &key).await {
        Ok(true) => {
            let ip = source_ip(&req, &state);
            audit::log(&state.registry, AuditEntry {
                app_id: Some(id),
                creator_id: None,
                actor_user_id: Some(authz.principal_id),
                actor_token_id: authz.token_id,
                action: Action::DeleteVar,
                resource: Some(&key),
                source_ip: ip.as_deref(),
            }).await;
            web::HttpResponse::NoContent().finish()
        }
        Ok(false) => web::HttpResponse::NotFound().json(&serde_json::json!({"error": "not found"})),
        Err(e) => env_err_response(e),
    }
}

// ------------------------------------------------------------------
// Secrets
// ------------------------------------------------------------------

pub async fn list_secrets(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await { return r; }
    let Ok(id) = Uuid::parse_str(&path) else { return bad_uuid(); };
    if let Err(resp) = authz
        .require(AuthzAction::SecretsRead, Resource::App { id: id.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.env_store.list_secret_names(id).await {
        Ok(names) => web::HttpResponse::Ok().json(&serde_json::json!({"secrets": names})),
        Err(e) => env_err_response(e),
    }
}

pub async fn set_secret(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<SetKv>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await { return r; }
    let Ok(id) = Uuid::parse_str(&path) else { return bad_uuid(); };
    if let Err(resp) = authz
        .require(AuthzAction::SecretsWrite, Resource::App { id: id.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.env_store.set_secret(id, &body.key, &body.value).await {
        Ok(()) => {
            let ip = source_ip(&req, &state);
            audit::log(&state.registry, AuditEntry {
                app_id: Some(id),
                creator_id: None,
                actor_user_id: Some(authz.principal_id),
                actor_token_id: authz.token_id,
                action: Action::SetSecret,
                resource: Some(&body.key),
                source_ip: ip.as_deref(),
            }).await;
            web::HttpResponse::NoContent().finish()
        }
        Err(e) => env_err_response(e),
    }
}

// ------------------------------------------------------------------
// Expose list (per-app `process.env` opt-in for secrets)
// ------------------------------------------------------------------

/// `GET /api/apps/:id/env/expose` — the current opt-in list of secret
/// names that surface in `process.env`. Names only — values come from
/// the underlying `app_secrets` table.
pub async fn list_expose(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await { return r; }
    let Ok(id) = Uuid::parse_str(&path) else { return bad_uuid(); };
    if let Err(resp) = authz
        .require(AuthzAction::SecretsRead, Resource::App { id: id.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.env_store.list_expose(id).await {
        Ok(keys) => web::HttpResponse::Ok().json(&serde_json::json!({"expose": keys})),
        Err(e) => env_err_response(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct SetExposeBody {
    /// Replacement list of secret names. Empty array clears the list.
    /// Each name must match the `valid_key` regex (UPPER_SNAKE_CASE);
    /// duplicates are deduplicated server-side.
    pub keys: Vec<String>,
}

/// `PUT /api/apps/:id/env/expose` — replace the opt-in list of secret
/// names that surface in `process.env`. Sticky across deploys (lives on
/// the env config, not the manifest). Empty array clears the list.
///
/// Why a separate endpoint: this is per-app metadata orthogonal to
/// any individual var/secret CRUD — extending POST /vars or
/// POST /secrets would entangle two unrelated concerns. The list
/// references secret names but doesn't store secret VALUES, so the
/// authorization model is the same as listing secret names (admin
/// auth, audited).
pub async fn set_expose(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<SetExposeBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await { return r; }
    let Ok(id) = Uuid::parse_str(&path) else { return bad_uuid(); };
    if let Err(resp) = authz
        .require(AuthzAction::SecretsWrite, Resource::App { id: id.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.env_store.set_expose(id, &body.keys).await {
        Ok(applied) => {
            // Audit: log the new list (joined with commas) as the
            // resource string. The full set is recoverable from
            // `app_env_expose` if needed; this gives ops a one-glance
            // record of what changed.
            let resource = applied.join(",");
            let ip = source_ip(&req, &state);
            audit::log(&state.registry, AuditEntry {
                app_id: Some(id),
                creator_id: None,
                actor_user_id: Some(authz.principal_id),
                actor_token_id: authz.token_id,
                action: Action::SetEnvExpose,
                resource: Some(&resource),
                source_ip: ip.as_deref(),
            }).await;
            web::HttpResponse::Ok().json(&serde_json::json!({"expose": applied}))
        }
        Err(e) => env_err_response(e),
    }
}

// ------------------------------------------------------------------
// Audit log read
// ------------------------------------------------------------------

/// `GET /api/apps/:id/audit?limit=N` — newest-first audit entries.
/// Master-key auth + admin rate limit. Limit clamped 1..=500 inside
/// `audit::recent_for_app` so callers can't ask for a million rows.
pub async fn list_audit(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    query: web::types::Query<AuditQuery>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await { return r; }
    let Ok(id) = Uuid::parse_str(&path) else { return bad_uuid(); };
    if let Err(resp) = authz
        .require(AuthzAction::AppsRead, Resource::App { id: id.to_string() }, &state)
        .await
    {
        return resp;
    }
    let limit = query.limit.unwrap_or(50);
    match audit::recent_for_app(&state.registry, id, limit).await {
        Ok(rows) => web::HttpResponse::Ok().json(&serde_json::json!({
            "audit": rows.iter().map(|r| serde_json::json!({
                "id": r.id.to_string(),
                "actor_user_id": r.actor_user_id.map(|id| id.to_string()),
                "actor_token_id": r.actor_token_id.map(|id| id.to_string()),
                "action": r.action,
                "resource": r.resource,
                "source_ip": r.source_ip,
                "at": r.at,
            })).collect::<Vec<_>>(),
            "count": rows.len(),
            "limit": limit.clamp(1, 500),
        })),
        Err(e) => {
            tracing::error!(error = %e, "control: audit query error");
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": "internal error"}))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    pub limit: Option<i64>,
}

pub async fn delete_secret(
    req: web::HttpRequest,
    path: Path<(String, String)>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = admin_rate_limit(&req, &state).await { return r; }
    let (id_s, key) = path.into_inner();
    let Ok(id) = Uuid::parse_str(&id_s) else { return bad_uuid(); };
    if let Err(resp) = authz
        .require(AuthzAction::SecretsWrite, Resource::App { id: id.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.env_store.delete_secret(id, &key).await {
        Ok(true) => {
            let ip = source_ip(&req, &state);
            audit::log(&state.registry, AuditEntry {
                app_id: Some(id),
                creator_id: None,
                actor_user_id: Some(authz.principal_id),
                actor_token_id: authz.token_id,
                action: Action::DeleteSecret,
                resource: Some(&key),
                source_ip: ip.as_deref(),
            }).await;
            web::HttpResponse::NoContent().finish()
        }
        Ok(false) => web::HttpResponse::NotFound().json(&serde_json::json!({"error": "not found"})),
        Err(e) => env_err_response(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_mutation_payload_cap_matches_rejected_body_budget() {
        assert_eq!(ENV_MUTATION_PAYLOAD_BYTES, 80 * 1024);
    }
}
