//! Internal API handlers — worker-facing endpoints.

use std::sync::Arc;

use ntex::web;
use ntex::web::types::{Json, Path, State};
use uuid::Uuid;

use appbase_common::types::UsageReport;
use crate::AppState;

// ---------------------------------------------------------------------------
// Auth helper
// ---------------------------------------------------------------------------

fn check_auth(req: &web::HttpRequest, state: &AppState) -> Option<web::HttpResponse> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = appbase_common::auth::extract_bearer(header);
    match token {
        Some(key) if appbase_common::auth::validate_control_key(key, &state.control_key) => None,
        _ => Some(
            web::HttpResponse::Unauthorized()
                .json(&serde_json::json!({"error":"unauthorized"})),
        ),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn health() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&serde_json::json!({"status":"ok"}))
}

pub async fn get_versions(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    match state.registry.get_versions().await {
        Ok(versions) => web::HttpResponse::Ok().json(&versions),
        Err(e) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": e.to_string()})),
    }
}

pub async fn get_bundle(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    app_id: Path<String>,
) -> web::HttpResponse {
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    let uid = match app_id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    match state.vfs.get(&uid.to_string()) {
        Ok(data) => web::HttpResponse::Ok()
            .content_type("application/octet-stream")
            .body(data),
        Err(appbase_common::vfs::VfsError::NotFound(_)) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"bundle not found"}))
        }
        Err(e) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": e.to_string()})),
    }
}

pub async fn get_routes(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    match state.registry.get_routes().await {
        Ok(routes) => web::HttpResponse::Ok().json(&routes),
        Err(e) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": e.to_string()})),
    }
}

/// POST /internal/usage — accept usage report from workers.
/// Uses common::types::UsageReport { worker_id, counters: { app_id → AppUsage } }
pub async fn report_usage(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    body: Json<UsageReport>,
) -> web::HttpResponse {
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    for (app_id, usage) in &body.counters {
        let deltas = [
            ("requests", usage.requests as i64),
            ("cpu_us", usage.cpu_us as i64),
            ("wall_us", usage.wall_us as i64),
            ("egress_bytes", usage.egress_bytes as i64),
            ("ingress_bytes", usage.ingress_bytes as i64),
        ];
        for (resource, delta) in &deltas {
            if *delta > 0 {
                if let Err(e) = state.registry.record_usage(app_id, resource, *delta).await {
                    return web::HttpResponse::InternalServerError()
                        .json(&serde_json::json!({"error": e.to_string()}));
                }
            }
        }
    }
    web::HttpResponse::Ok().json(&serde_json::json!({"recorded": true}))
}
