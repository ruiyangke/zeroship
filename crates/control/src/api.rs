//! Admin API handlers — app CRUD, deploy, plan, usage.

use std::sync::Arc;

use ntex::web;
use ntex::util::Bytes;
use ntex::web::types::{Json, Path, State};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::registry::RegistryError;
use crate::AppState;

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateAppBody {
    pub name: String,
    #[serde(default = "default_plan")]
    pub plan_id: String,
}

fn default_plan() -> String {
    "free".to_string()
}

#[derive(Deserialize)]
pub struct SetPlanBody {
    pub plan_id: String,
}

// ---------------------------------------------------------------------------
// Admin auth — require master key on all mutating endpoints
// ---------------------------------------------------------------------------

fn check_admin_auth(req: &web::HttpRequest, state: &AppState) -> Option<web::HttpResponse> {
    if state.master_key.is_empty() {
        return None; // No master key configured — allow all (dev mode)
    }
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = appbase_common::auth::extract_bearer(header);
    match token {
        Some(key) if appbase_common::auth::validate_control_key(key, &state.master_key) => None,
        _ => Some(
            web::HttpResponse::Unauthorized()
                .json(&serde_json::json!({"error":"unauthorized — master key required"})),
        ),
    }
}

// ---------------------------------------------------------------------------
// Error → HttpResponse
// ---------------------------------------------------------------------------

fn error_response(e: RegistryError) -> web::HttpResponse {
    match e {
        RegistryError::NotFound(msg) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({ "error": msg }))
        }
        RegistryError::AlreadyExists(msg) => {
            web::HttpResponse::Conflict().json(&serde_json::json!({ "error": msg }))
        }
        RegistryError::InvalidInput(msg) => {
            web::HttpResponse::BadRequest().json(&serde_json::json!({ "error": msg }))
        }
        RegistryError::Database(msg) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({ "error": msg })),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn create_app(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    body: Json<CreateAppBody>,
) -> web::HttpResponse {
    if let Some(resp) = check_admin_auth(&req, &state) { return resp; }
    match state.registry.create_app(&body.name, &body.plan_id).await {
        Ok(record) => {
            // Include api_key in the create response (it's skipped from normal serialization)
            let mut json = serde_json::to_value(&record).unwrap();
            json["api_key"] = serde_json::Value::String(record.api_key.clone());
            web::HttpResponse::Created().json(&json)
        }
        Err(e) => error_response(e),
    }
}

pub async fn list_apps(state: State<Arc<AppState>>) -> web::HttpResponse {
    match state.registry.list_apps().await {
        Ok(apps) => web::HttpResponse::Ok().json(&apps),
        Err(e) => error_response(e),
    }
}

pub async fn get_app(state: State<Arc<AppState>>, id: Path<String>) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    match state.registry.get_app(&uid).await {
        Ok(Some(record)) => web::HttpResponse::Ok().json(&record),
        Ok(None) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
    }
}

pub async fn delete_app(req: web::HttpRequest, state: State<Arc<AppState>>, id: Path<String>) -> web::HttpResponse {
    if let Some(resp) = check_admin_auth(&req, &state) { return resp; }
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    // Delete from VFS first (ignore NotFound — bundle may not exist yet).
    let app_id_str = uid.to_string();
    if let Err(e) = state.vfs.delete(&app_id_str) {
        match e {
            appbase_common::vfs::VfsError::NotFound(_) => { /* ok */ }
            other => {
                return web::HttpResponse::InternalServerError()
                    .json(&serde_json::json!({"error": other.to_string()}));
            }
        }
    }
    match state.registry.delete_app(&uid).await {
        Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({"deleted": true})),
        Ok(false) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
    }
}

pub async fn deploy(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    id: Path<String>,
    body: Bytes,
) -> web::HttpResponse {
    if let Some(resp) = check_admin_auth(&req, &state) { return resp; }
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };

    // Compute SHA-256 hash of the bundle bytes.
    let mut hasher = Sha256::new();
    hasher.update(&body);
    let deploy_hash = hex::encode(hasher.finalize());

    // Store in VFS.
    let app_id_str = uid.to_string();
    if let Err(e) = state.vfs.put(&app_id_str, &body) {
        return web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": e.to_string()}));
    }

    // Update deploy_hash in DB.
    match state.registry.set_deploy_hash(&uid, &deploy_hash).await {
        Ok(true) => {
            web::HttpResponse::Ok().json(&serde_json::json!({"deploy_hash": deploy_hash}))
        }
        Ok(false) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
    }
}

pub async fn set_plan(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    id: Path<String>,
    body: Json<SetPlanBody>,
) -> web::HttpResponse {
    if let Some(resp) = check_admin_auth(&req, &state) { return resp; }
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    match state.registry.set_plan(&uid, &body.plan_id).await {
        Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({"updated": true})),
        Ok(false) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
    }
}

pub async fn get_usage(state: State<Arc<AppState>>, id: Path<String>) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    match state.registry.get_usage(&uid).await {
        Ok(usage) => web::HttpResponse::Ok().json(&usage),
        Err(e) => error_response(e),
    }
}
