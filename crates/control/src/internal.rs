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

/// GET /internal/assets/{app_id}/{path:.*} — serve a static asset to the gateway.
///
/// TODO(phase 4): remove after gateway switches to BlobStore directly.
/// Today this exists so the gateway still has a working asset path
/// while phase 4 migrates `crates/gateway/src/router.rs` away from the
/// legacy `/internal/assets/...` HTTP fetch.
///
/// Resolution order matches `crates/gateway/src/compiled.rs::lookup_asset`:
/// `runtime_assets` wins on conflict, then `assets`. The hash from
/// either map keys into `blob_store.get_blob`. The `content_type` on
/// the response is what the manifest carries (not a sniffed extension)
/// — that matches what the gateway will emit once it reads from
/// `BlobStore` directly.
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

    // Normalise the asset path to a leading-slash URL form. The gateway
    // sends e.g. `index.html`, but the manifest keys are `/index.html`.
    let lookup_path = if asset_path.starts_with('/') {
        asset_path.clone()
    } else {
        format!("/{asset_path}")
    };

    // Pull the manifest JSON and look the asset up. NULL → 404 (the app
    // hasn't deployed yet); parse failure → 500 (corrupt row).
    let manifest_json = match state.registry.get_manifest_json(&uid).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return web::HttpResponse::NotFound()
                .json(&serde_json::json!({"error": "asset not found"}));
        }
        Err(e) => {
            return web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": e.to_string()}));
        }
    };
    let manifest: zeroship_core::types::Manifest = match serde_json::from_str(&manifest_json) {
        Ok(m) => m,
        Err(e) => {
            return web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": format!("manifest parse: {e}")}));
        }
    };

    let entry = manifest
        .runtime_assets
        .get(&lookup_path)
        .or_else(|| manifest.assets.get(&lookup_path));
    let Some(entry) = entry else {
        return web::HttpResponse::NotFound()
            .json(&serde_json::json!({"error": "asset not found"}));
    };

    match state.blob_store.get_blob(&entry.hash).await {
        Ok(bytes) => {
            // bytes::Bytes -> Vec<u8> for ntex's body — phase 4 will let
            // the gateway mmap directly and skip this copy entirely.
            let body: Vec<u8> = bytes.to_vec();
            web::HttpResponse::Ok()
                .content_type(entry.content_type.clone())
                .body(body)
        }
        Err(zeroship_core::BlobError::NotFound(_)) => web::HttpResponse::NotFound()
            .json(&serde_json::json!({"error": "asset blob missing"})),
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
