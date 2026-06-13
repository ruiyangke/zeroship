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
            tracing::warn!(method = %req.method(), path = %req.path(), "control-internal: auth rejected");
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

/// Worker-authenticated: return the merged env for a given app as a
/// JSON object in the split `{ vars, secrets, expose }` shape. Workers
/// call this on bundle load and cache the result per-thread.
///
/// The split shape is the contract the runtime expects (see
/// `crates/runtime/src/fetch_outcome.rs::EnvSnapshot`): vars are always
/// in `process.env`, secrets are NOT in `process.env` unless their name
/// is in the per-app `expose` list, and both are visible via
/// `import { env } from "zeroship"` and `env.get(name)`.
pub async fn get_app_env(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    app_id: Path<String>,
) -> web::HttpResponse {
    // Internal endpoint, no user authz: workers authenticate with the
    // control-key shared secret and there is no user principal.
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    let Ok(id) = Uuid::parse_str(&app_id) else {
        return web::HttpResponse::BadRequest()
            .json(&serde_json::json!({"error": "bad app_id"}));
    };
    match state.env_store.merged_env_for_worker(id).await {
        Ok(value) => web::HttpResponse::Ok().json(&value),
        Err(crate::env_store::EnvError::AppNotFound) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => {
            tracing::error!(app_id = %id, error = %e, "control-internal: env fetch error");
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error":"internal error"}))
        }
    }
}

pub async fn get_versions(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    // Internal endpoint, no user authz: workers authenticate with the
    // control-key shared secret and there is no user principal.
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
    // Internal endpoint, no user authz: workers authenticate with the
    // control-key shared secret and there is no user principal.
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
    // Internal endpoint, no user authz: gateways authenticate with the
    // control-key shared secret and there is no user principal.
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    match state.registry.get_routes().await {
        Ok(routes) => web::HttpResponse::Ok().json(&routes),
        Err(e) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": e.to_string()})),
    }
}

/// POST /internal/usage — accept a usage report from a worker.
///
/// `UsageReport { worker_id, report_id, sequence, counters: { app_id →
/// AppUsage } }`. Ingest is IDEMPOTENT: the report is deduped on
/// `(worker_id, sequence)` and aggregated per `(app_id, calendar-month,
/// metric)` into `zeroship.usage_aggregates`. A duplicate (an at-least-once
/// producer retry) is a no-op — it never double-counts. The response always
/// carries the worker's `high_water` sequence so a producer can resync after
/// a restart, plus a `duplicate` flag.
pub async fn report_usage(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    body: Json<UsageReport>,
) -> web::HttpResponse {
    // Internal endpoint, no user authz: workers authenticate with the
    // control-key shared secret and there is no user principal.
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    let metering = crate::metering::Metering::new(state.registry.clone());
    match metering.ingest(&body).await {
        Ok(outcome) => web::HttpResponse::Ok().json(&serde_json::json!({
            "recorded": true,
            "duplicate": outcome.duplicate,
            "high_water": outcome.high_water_sequence,
        })),
        Err(e) => {
            tracing::error!(
                worker_id = %body.worker_id,
                sequence = body.sequence,
                error = %e,
                "control-internal: usage ingest failed"
            );
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": e.to_string()}))
        }
    }
}
