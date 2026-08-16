//! Internal API handlers — worker-facing endpoints.

use std::sync::Arc;

use ntex::web;
use ntex::web::types::{Path, State};
use uuid::Uuid;
use zeroship_core::readiness::ReadinessGate;

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
        // Empty control_key never authenticates. Otherwise an unauthenticated
        // GET to /internal/* could leak decrypted secrets to the network.
        Some(key)
            if !state.control_key.is_empty()
                && zeroship_core::auth::constant_time_eq(key, state.control_key.expose_secret()) =>
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

/// Liveness. Constant 200 by design: it answers "this process is running and
/// its event loop is not wedged" and MUST NOT touch a dependency. A liveness
/// probe that fails when Postgres blips gets the container killed for someone
/// else's outage.
pub async fn healthz() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&serde_json::json!({"ok": true}))
}

/// Readiness. The control plane cannot serve a single API call without
/// Postgres, so this probes the SHARED long-lived `control_pg` client with a
/// protocol-level sync - no new connection, no query planning, no table read.
///
/// Bounded, cached and single-flighted by [`ReadinessGate`]; the body carries
/// no DSN, host, or driver error text.
pub async fn readyz(
    state: State<Arc<AppState>>,
    gate: State<Arc<ReadinessGate>>,
) -> web::HttpResponse {
    let ready = gate
        .ready(|| async {
            match state.control_pg.check_connection().await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(error = %error, "control readiness: postgres unreachable");
                    false
                }
            }
        })
        .await;
    if ready {
        web::HttpResponse::Ok().json(&serde_json::json!({"ready": true}))
    } else {
        web::HttpResponse::ServiceUnavailable().json(&serde_json::json!({"ready": false}))
    }
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
    match state.registry.get_gateway_snapshot().await {
        Ok(snapshot) => web::HttpResponse::Ok().json(&snapshot),
        Err(e) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": e.to_string()})),
    }
}

/// POST /internal/billing/reconcile?period=<unix-seconds> — operator-gated
/// on-demand trigger of the billing reconciler for a SPECIFIC closed period.
///
/// Same gate as every other `/internal/*` endpoint ([`check_auth`]): the
/// control-key shared secret. Without a valid control-key bearer it 401s exactly
/// like the operator reconcile endpoints.
///
/// The production reconcile cron only ever bills the PREVIOUS calendar month
/// (`previous_period_start_unix(now)`), which an end-to-end test cannot wait a
/// month for. This endpoint drives the SAME [`crate::cron::billing_reconcile::tick_with`]
/// sweep against a caller-chosen `period` so a harness (or an operator
/// re-running a missed close) can reconcile a specific closed period on demand.
/// Idempotency is unchanged: the `billing_runs` / `billing_run_items` guards
/// make a repeat trigger for the same period a no-op.
///
/// `period` is the unix-seconds start of the calendar month to bill. We pass it
/// as `now` to `tick_with`, which derives the period it bills as
/// `previous_period_start_unix(now)` — so the caller passes a timestamp in the
/// month AFTER the one they want billed (mirroring how the cron, ticking in
/// month M, bills M-1). The response echoes the resolved `period_start` so the
/// caller can assert which period was reconciled.
pub async fn force_reconcile(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    // `?period=<unix-seconds>` — the `now` instant to reconcile against. Default
    // to the live wall clock (bills the previous calendar month, like the cron).
    let now_unix: i64 = req
        .query_string()
        .split('&')
        .find_map(|kv| kv.strip_prefix("period="))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or_else(|| chrono::Utc::now().timestamp());

    let stripe = crate::stripe_client::StripeClient::new(crate::SecretString::new(
        state.stripe_secret_key.expose_secret().to_string(),
    ))
    .with_base_url(state.stripe_base_url.clone());

    match crate::cron::billing_reconcile::tick_with(&state, &stripe, now_unix).await {
        Ok(billed) => {
            let period_start = crate::cron::billing_reconcile::previous_period_start_unix(now_unix);
            web::HttpResponse::Ok().json(&serde_json::json!({
                "billed": billed,
                "period_start": period_start,
                "now": now_unix,
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, now_unix, "control-internal: force_reconcile failed");
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": e.to_string()}))
        }
    }
}

/// POST /internal/spend/reconcile — operator-gated on-demand trigger of ONE
/// spend-reconcile sweep ([`crate::cron::spend_reconcile::tick`]).
///
/// Same gate as every other `/internal/*` endpoint ([`check_auth`]). The
/// spend cron runs every ~60s on its own; this lets an operator (or an E2E)
/// force a single sweep immediately so the derived [`zeroship_core::types::SpendState`] is persisted
/// without waiting a full tick. The gateway still picks the new state up on its
/// next `/internal/routes` poll (decision D1) — this endpoint only advances the
/// CONTROL-side derivation, it does not push to the gateway. Idempotent: a
/// no-op sweep simply reports `transitions: 0`.
pub async fn force_spend_reconcile(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = check_auth(&req, &state) {
        return resp;
    }
    match crate::cron::spend_reconcile::tick(&state).await {
        Ok(transitions) => web::HttpResponse::Ok().json(&serde_json::json!({
            "transitions": transitions,
        })),
        Err(e) => {
            tracing::error!(error = %e, "control-internal: force_spend_reconcile failed");
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": e.to_string()}))
        }
    }
}
