//! Internal API handlers — worker-facing endpoints.

use std::sync::Arc;

use ntex::web;
use ntex::web::types::{Json, Path, State};
use uuid::Uuid;

use zeroship_core::types::UsageReport;
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
    let token = zeroship_core::auth::extract_bearer(header);
    match token {
        Some(key) if zeroship_core::auth::validate_control_key(key, &state.control_key) => None,
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
        Err(zeroship_core::vfs::VfsError::NotFound(_)) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"bundle not found"}))
        }
        Err(e) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": e.to_string()})),
    }
}

pub async fn get_app_version(
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
    match state.registry.get_versions().await {
        Ok(versions) => match versions.get(&uid) {
            Some(info) => web::HttpResponse::Ok().json(info),
            None => web::HttpResponse::NotFound()
                .json(&serde_json::json!({"error":"app not found"})),
        },
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

// ---------------------------------------------------------------------------
// Asset serving
// ---------------------------------------------------------------------------

/// Return the MIME content type for a file extension.
fn content_type_for_ext(path: &str) -> &'static str {
    if let Some(ext) = path.rsplit('.').next() {
        match ext.to_ascii_lowercase().as_str() {
            "html" => "text/html; charset=utf-8",
            "js" | "mjs" => "application/javascript; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "json" => "application/json; charset=utf-8",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "svg" => "image/svg+xml",
            "ico" => "image/x-icon",
            "woff" => "font/woff",
            "woff2" => "font/woff2",
            "ttf" => "font/ttf",
            "webp" => "image/webp",
            "txt" => "text/plain; charset=utf-8",
            "xml" => "application/xml; charset=utf-8",
            "webmanifest" => "application/manifest+json",
            _ => "application/octet-stream",
        }
    } else {
        "application/octet-stream"
    }
}

/// GET /internal/assets/{app_id}/{path:.*} — serve a static asset to the gateway.
pub async fn get_asset(
    state: State<Arc<AppState>>,
    path: web::types::Path<(String, String)>,
) -> web::HttpResponse {
    let (app_id, asset_path) = path.into_inner();

    // No auth required — the gateway calls this, and static assets are public.
    let uid = match app_id.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };

    let app_id_str = uid.to_string();
    match state.vfs.get_asset(&app_id_str, &asset_path) {
        Ok(data) => {
            let ct = content_type_for_ext(&asset_path);
            web::HttpResponse::Ok()
                .content_type(ct)
                .body(data)
        }
        Err(zeroship_core::vfs::VfsError::NotFound(_)) => {
            web::HttpResponse::NotFound()
                .json(&serde_json::json!({"error":"asset not found"}))
        }
        Err(e) => {
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": e.to_string()}))
        }
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
