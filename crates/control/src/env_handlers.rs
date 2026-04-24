//! Admin API for per-app vars + secret names.
//!
//! Mutations require master key (same auth as the rest of the admin API).
//! Secrets are write-only over this surface — GET returns names and
//! `updated_at` timestamps, never values. Rotation = PUT a fresh value.

use std::sync::Arc;

use ntex::web::{self, types::{Json, Path, State}};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;
use crate::env_store::EnvError;

fn bad_uuid() -> web::HttpResponse {
    web::HttpResponse::BadRequest().json(&serde_json::json!({"error": "bad app_id"}))
}

fn env_err_response(e: EnvError) -> web::HttpResponse {
    use EnvError::*;
    match &e {
        BadKey(_) => web::HttpResponse::BadRequest().json(&serde_json::json!({"error": e.to_string()})),
        Db(_) | Crypto(_) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": e.to_string()})),
    }
}

#[derive(Deserialize)]
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
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let Ok(id) = Uuid::parse_str(&path) else { return bad_uuid(); };
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
    body: Json<SetKv>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let Ok(id) = Uuid::parse_str(&path) else { return bad_uuid(); };
    match state.env_store.set_var(id, &body.key, &body.value).await {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(e) => env_err_response(e),
    }
}

pub async fn delete_var(
    req: web::HttpRequest,
    path: Path<(String, String)>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let (id_s, key) = path.into_inner();
    let Ok(id) = Uuid::parse_str(&id_s) else { return bad_uuid(); };
    match state.env_store.delete_var(id, &key).await {
        Ok(true) => web::HttpResponse::NoContent().finish(),
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
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let Ok(id) = Uuid::parse_str(&path) else { return bad_uuid(); };
    match state.env_store.list_secret_names(id).await {
        Ok(names) => web::HttpResponse::Ok().json(&serde_json::json!({"secrets": names})),
        Err(e) => env_err_response(e),
    }
}

pub async fn set_secret(
    req: web::HttpRequest,
    path: Path<String>,
    body: Json<SetKv>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let Ok(id) = Uuid::parse_str(&path) else { return bad_uuid(); };
    match state.env_store.set_secret(id, &body.key, &body.value).await {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(e) => env_err_response(e),
    }
}

pub async fn delete_secret(
    req: web::HttpRequest,
    path: Path<(String, String)>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let (id_s, key) = path.into_inner();
    let Ok(id) = Uuid::parse_str(&id_s) else { return bad_uuid(); };
    match state.env_store.delete_secret(id, &key).await {
        Ok(true) => web::HttpResponse::NoContent().finish(),
        Ok(false) => web::HttpResponse::NotFound().json(&serde_json::json!({"error": "not found"})),
        Err(e) => env_err_response(e),
    }
}
