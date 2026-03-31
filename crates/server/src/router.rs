//! Axum router for appbase — RPC dispatch with metering, quota enforcement, and admin API.

use appbase_core::config::AppbaseConfig;
use appbase_core::plugin::PluginFactory;
use appbase_core::types::AppBundle;
use appbase_isolate::pool::IsolatePool;
use appbase_metering::enforcer::{self, QuotaDecision};
use appbase_metering::meter::{MeterRegistry, UsageDelta};
use appbase_metering::plan::QuotaPlan;
use appbase_metering::rate_limit::RateLimiter;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::Router;
use bytes::Bytes;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::middleware;

/// Shared state for all axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<IsolatePool>,
    pub bundles: Arc<Mutex<HashMap<String, AppBundle>>>,
    pub default_app: String,
    pub static_html: Option<Bytes>,
    /// Metering: per-app usage counters.
    pub meters: Arc<MeterRegistry>,
    /// Rate limiter: per-app requests/second.
    pub rate_limiter: Arc<RateLimiter>,
}

/// Build the axum router with all routes and middleware.
pub fn build(state: AppState) -> Router {
    let (cors, _compression) = middleware::production_layers();

    Router::new()
        .route("/rpc", post(handle_rpc))
        .route("/_stats", get(handle_stats))
        .route("/_health", get(handle_health))
        .route("/_apps/{app_id}", delete(handle_evict_app))
        .route("/_apps/{app_id}/usage", get(handle_app_usage))
        .route("/_usage", get(handle_all_usage))
        .fallback(get(handle_static))
        .layer(cors)
        .with_state(state)
}

/// Create an AppState for single-app mode.
///
/// If `plan` is provided, it is used for metering; otherwise defaults to unlimited (dev-friendly).
pub fn single_app_state(
    server_js: String,
    client_html: Option<Vec<u8>>,
    config: &AppbaseConfig,
    data_dir: PathBuf,
    plugin_factory: PluginFactory,
    plan: Option<QuotaPlan>,
) -> AppState {
    let pool = IsolatePool::new(config.isolates.clone(), data_dir, plugin_factory);

    let mut bundles = HashMap::new();
    bundles.insert(
        "default".to_string(),
        AppBundle {
            server_js,
            client_html: client_html.clone(),
        },
    );

    let default_plan = plan.unwrap_or_else(QuotaPlan::unlimited);
    let meters = Arc::new(MeterRegistry::new(default_plan));
    let rate_limiter = Arc::new(RateLimiter::new(10000, 50000));

    AppState {
        pool,
        bundles: Arc::new(Mutex::new(bundles)),
        default_app: "default".to_string(),
        static_html: client_html.map(Bytes::from),
        meters,
        rate_limiter,
    }
}

/// Start the HTTP server.
pub async fn serve(state: AppState, host: &str, port: u16) -> Result<(), String> {
    let app = build(state);
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("Failed to bind {addr}: {e}"))?;

    eprintln!("[appbase] http://{addr}");

    axum::serve(listener, app)
        .await
        .map_err(|e| format!("Server error: {e}"))
}

// --- Handlers ---

/// POST /rpc — dispatch with quota check + metering + response headers.
async fn handle_rpc(State(state): State<AppState>, body: String) -> Response {
    let app_id = state.default_app.clone();

    // 1. Rate limit check
    if !state.rate_limiter.check(&app_id) {
        return json_response_with_status(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"jsonrpc":"2.0","error":{"code":-32429,"message":"Rate limit exceeded"},"id":null}"#,
        );
    }

    // 2. Quota check
    let meter = state.meters.get_or_create(&app_id);
    match enforcer::check_quota(&meter, &meter.plan) {
        QuotaDecision::Deny(denial) => {
            return json_response_with_status(
                StatusCode::TOO_MANY_REQUESTS,
                &format!(
                    r#"{{"jsonrpc":"2.0","error":{{"code":-32429,"message":"{}","data":{{"dimension":"{}","used":{},"limit":{}}}}},"id":null}}"#,
                    denial.message, denial.dimension, denial.used, denial.limit
                ),
            );
        }
        QuotaDecision::Allow | QuotaDecision::Warn(_) => {} // proceed
    }

    // 3. Get app bundle
    let bundle = {
        let bundles = state.bundles.lock().unwrap();
        match bundles.get(&app_id) {
            Some(b) => b.clone(),
            None => {
                return json_response(
                    StatusCode::NOT_FOUND,
                    r#"{"error":"App not found"}"#,
                )
            }
        }
    };

    // 4. Dispatch to V8
    let wall_start = std::time::Instant::now();
    let result = state
        .pool
        .dispatch(&app_id, &bundle.server_js, body)
        .await;
    let wall_time = wall_start.elapsed();

    match result {
        Ok(rpc_result) => {
            let cpu_ms = rpc_result.cpu_time.as_secs_f64() * 1000.0;
            let response_bytes = rpc_result.json.len() as u64;

            // 5. Record usage
            meter.record(&UsageDelta {
                cpu_time: rpc_result.cpu_time,
                wall_time,
                egress_bytes: response_bytes,
                ..UsageDelta::default()
            });

            // 6. Build response with metering + IETF RateLimit headers
            let snapshot = meter.snapshot();
            let mut headers = HeaderMap::new();

            // Custom metering headers
            add_header(&mut headers, "x-cpu-time-ms", &format!("{cpu_ms:.2}"));
            add_header(
                &mut headers,
                "x-wall-time-ms",
                &format!("{:.2}", wall_time.as_secs_f64() * 1000.0),
            );

            // IETF RateLimit headers (draft-ietf-httpapi-ratelimit-headers-10)
            if let Some(quota) = meter.plan.quotas.get("requests") {
                if let Some(limit) = quota.max {
                    let remaining = limit.saturating_sub(snapshot.requests);
                    // Approximate seconds until monthly reset (simplified)
                    let reset = 30 * 24 * 3600; // ~30 days
                    add_header(
                        &mut headers,
                        "ratelimit",
                        &format!("limit={limit}, remaining={remaining}, reset={reset}"),
                    );
                    add_header(
                        &mut headers,
                        "ratelimit-policy",
                        &format!("{limit};w={reset}"),
                    );
                }
            }

            let mut response = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(rpc_result.json))
                .unwrap();
            response.headers_mut().extend(headers);
            response
        }
        Err(e) => {
            let safe = sanitize_error(&e);
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!(
                    r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"{safe}"}},"id":null}}"#
                ),
            )
        }
    }
}

/// GET /_stats — pool statistics.
async fn handle_stats(State(state): State<AppState>) -> Response {
    let stats = state.pool.stats();
    json_response(
        StatusCode::OK,
        &serde_json::to_string(&stats).unwrap_or_default(),
    )
}

/// GET /_usage — all apps' usage.
async fn handle_all_usage(State(state): State<AppState>) -> Response {
    let usage = state.meters.all_usage();
    json_response(
        StatusCode::OK,
        &serde_json::to_string(&usage).unwrap_or_default(),
    )
}

/// GET /_apps/{app_id}/usage — single app's usage.
async fn handle_app_usage(
    State(state): State<AppState>,
    Path(app_id): Path<String>,
) -> Response {
    match state.meters.get_usage(&app_id) {
        Some(usage) => json_response(
            StatusCode::OK,
            &serde_json::to_string(&usage).unwrap_or_default(),
        ),
        None => json_response(
            StatusCode::NOT_FOUND,
            &format!(r#"{{"error":"No usage data for '{app_id}'"}}"#),
        ),
    }
}

/// DELETE /_apps/{app_id} — manually evict + clear meter.
async fn handle_evict_app(
    State(state): State<AppState>,
    Path(app_id): Path<String>,
) -> Response {
    state.pool.evict_app(&app_id);
    state.meters.remove(&app_id);
    state.rate_limiter.remove(&app_id);
    json_response(
        StatusCode::OK,
        &format!(r#"{{"evicted":"{app_id}"}}"#),
    )
}

/// GET /_health — health check.
async fn handle_health() -> Response {
    json_response(StatusCode::OK, r#"{"status":"ok"}"#)
}

/// Fallback — serve static HTML.
async fn handle_static(State(state): State<AppState>) -> Response {
    if let Some(ref html) = state.static_html {
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(html.clone()))
            .unwrap()
    } else {
        json_response(StatusCode::OK, r#"{"status":"appbase running"}"#)
    }
}

// --- Helpers ---

fn json_response(status: StatusCode, body: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn json_response_with_status(status: StatusCode, body: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header("Retry-After", "1")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn add_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    let n = axum::http::header::HeaderName::from_static(name);
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.insert(n, v);
    }
}

fn sanitize_error(msg: &str) -> String {
    msg.lines()
        .next()
        .unwrap_or("Internal error")
        .replace('/', "")
        .replace('\\', "")
}
