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
    if state.insecure_dev {
        return None;
    }
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = zeroship_core::auth::extract_bearer(header);
    match token {
        // Empty control_key still requires a bearer token OR insecure_dev —
        // otherwise an unauthenticated GET to /internal/* leaks decrypted
        // secrets to anyone on the network.
        Some(key)
            if !state.control_key.is_empty()
                && zeroship_core::auth::validate_control_key(key, state.control_key.expose_secret()) =>
        {
            None
        }
        _ => {
            eprintln!("[control-internal] auth rejected on {} {}", req.method(), req.path());
            Some(
                web::HttpResponse::Unauthorized()
                    .json(&serde_json::json!({"error":"unauthorized"})),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn health() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&serde_json::json!({"status":"ok"}))
}

/// Worker-authenticated: return the merged env (vars + decrypted secrets)
/// for a given app as a JSON object. Workers call this on bundle load
/// and cache the result per-thread.
pub async fn get_app_env(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    app_id: Path<String>,
) -> web::HttpResponse {
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    let Ok(id) = Uuid::parse_str(&app_id) else {
        return web::HttpResponse::BadRequest()
            .json(&serde_json::json!({"error": "bad app_id"}));
    };
    match state.env_store.merged_env(id).await {
        Ok(map) => web::HttpResponse::Ok().json(&serde_json::Value::Object(map)),
        Err(crate::env_store::EnvError::AppNotFound) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => {
            eprintln!("[control-internal] env fetch error for {id}: {e}");
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error":"internal error"}))
        }
    }
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
