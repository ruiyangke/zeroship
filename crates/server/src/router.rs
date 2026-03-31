//! Axum router for appbase — RPC dispatch with metering, quota enforcement, and admin API.

use appbase_core::config::AppbaseConfig;
use appbase_core::plugin::PluginFactory;
use appbase_core::types::AppBundle;
use appbase_isolate::pool::IsolatePool;
use appbase_metering::concurrency::ConcurrencyGuard;
use appbase_metering::enforcer::{self, QuotaDecision};
use appbase_metering::error_codes;
use appbase_metering::meter::{MeterRegistry, UsageDelta};
use appbase_metering::plan::QuotaPlan;
use appbase_metering::rate_limit::RateLimiter;
use std::sync::atomic::AtomicU32;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::Router;
use bytes::Bytes;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use crate::middleware;

/// Default concurrency limit per app (max in-flight requests).
const DEFAULT_CONCURRENCY_LIMIT: u32 = 100;

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
    /// Concurrency gauge: counts in-flight requests per app.
    pub concurrency_gauge: Arc<AtomicU32>,
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
        concurrency_gauge: Arc::new(AtomicU32::new(0)),
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

/// Compute seconds until next monthly period reset (1st of next month UTC).
fn seconds_until_period_reset() -> u64 {
    let now = time::OffsetDateTime::now_utc();
    let next_month = if now.month() == time::Month::December {
        time::Month::January
    } else {
        now.month().next()
    };
    let next_year = if now.month() == time::Month::December {
        now.year() + 1
    } else {
        now.year()
    };
    if let Ok(reset_date) = time::Date::from_calendar_date(next_year, next_month, 1) {
        let reset = reset_date.with_hms(0, 0, 0).unwrap().assume_utc();
        let diff = reset - now;
        diff.whole_seconds().max(1) as u64
    } else {
        30 * 24 * 3600 // fallback ~30 days
    }
}

/// POST /rpc — dispatch with quota check + metering + response headers.
///
/// Enforcement pipeline per spec §4.1:
/// 1. Rate limit → 2. Concurrency guard → 3. Spending limit →
/// 4. Quota check → 5. Dispatch → 6. Record usage → 7. Response headers
async fn handle_rpc(State(state): State<AppState>, body: String) -> Response {
    let app_id = state.default_app.clone();

    // 1. Rate limit check
    if !state.rate_limiter.check(&app_id) {
        return rpc_error_response(
            StatusCode::TOO_MANY_REQUESTS,
            error_codes::RATE_LIMITED,
            "Rate limit exceeded",
            r#""type":"rate_limited""#,
            Some(1), // retry after 1 second for rate limit
        );
    }

    // 2. Concurrency guard — RAII, auto-decrements on drop
    let _concurrency_guard = match ConcurrencyGuard::try_acquire(
        &state.concurrency_gauge,
        DEFAULT_CONCURRENCY_LIMIT,
    ) {
        Some(guard) => guard,
        None => {
            return rpc_error_response(
                StatusCode::TOO_MANY_REQUESTS,
                error_codes::CONCURRENCY_LIMIT,
                "Too many concurrent requests",
                r#""type":"concurrency_limit""#,
                Some(1),
            );
        }
    };

    // 3. Spending limit check
    let meter = state.meters.get_or_create(&app_id);
    if meter.spend_blocked.load(Ordering::Acquire) {
        let reset = seconds_until_period_reset();
        return rpc_error_response(
            StatusCode::TOO_MANY_REQUESTS,
            error_codes::SPENDING_LIMIT,
            "Spending limit reached",
            r#""type":"spending_limit""#,
            Some(reset),
        );
    }

    // 4. Quota check — single call, capture both deny and warnings
    let quota_decision = enforcer::check_quota(&meter, &meter.plan);
    let quota_warnings = match &quota_decision {
        QuotaDecision::Deny(denial) => {
            let reset = seconds_until_period_reset();
            return rpc_error_response(
                StatusCode::TOO_MANY_REQUESTS,
                denial.error_code,
                &denial.message,
                &format!(
                    r#""type":"quota_exceeded","dimension":"{}","used":{},"limit":{}"#,
                    denial.dimension, denial.used, denial.limit
                ),
                Some(reset),
            );
        }
        QuotaDecision::Warn(w) => w.clone(),
        QuotaDecision::Allow => vec![],
    };

    // 5. Get app bundle
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

    // 6. Dispatch to V8
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

            // 7. Record usage
            meter.record(&UsageDelta {
                cpu_time: rpc_result.cpu_time,
                wall_time,
                egress_bytes: response_bytes,
                ..UsageDelta::default()
            });

            // 7.5. Update spend accumulator and check spending limit
            // Approximate cost: $0.30 per million requests = 0.03 cents per request = 0.3 tenths per request
            let cost_tenths: u64 = 3; // simplified: ~0.3 tenths-of-a-cent per request
            meter.spend_accumulator_tenths.fetch_add(cost_tenths, Ordering::Release);
            if let Some(limit_cents) = meter.plan.spending_limit_cents {
                let spent_tenths = meter.spend_accumulator_tenths.load(Ordering::Acquire);
                if spent_tenths >= limit_cents * 10 {
                    meter.spend_blocked.store(true, Ordering::Release);
                }
            }

            // 8. Build response with metering + IETF RateLimit headers
            let snapshot = meter.snapshot();
            let reset_secs = seconds_until_period_reset();
            let mut headers = HeaderMap::new();

            // Custom metering headers
            add_header(&mut headers, "x-cpu-time-ms", &format!("{cpu_ms:.2}"));
            add_header(
                &mut headers,
                "x-wall-time-ms",
                &format!("{:.2}", wall_time.as_secs_f64() * 1000.0),
            );
            add_header(&mut headers, "x-plan", &meter.plan.name);

            // IETF RateLimit headers (draft-ietf-httpapi-ratelimit-headers-10)
            if let Some(quota) = meter.plan.quotas.get("requests") {
                if let Some(limit) = quota.max {
                    let remaining = limit.saturating_sub(snapshot.requests);
                    add_header(
                        &mut headers,
                        "ratelimit",
                        &format!("limit={limit}, remaining={remaining}, reset={reset_secs}"),
                    );
                    add_header(
                        &mut headers,
                        "ratelimit-policy",
                        &format!("{limit};w={reset_secs}"),
                    );
                }
            }

            // Quota warning headers (spec §8.2)
            for warning in &quota_warnings {
                add_header(
                    &mut headers,
                    "x-quota-warning",
                    &format!("{} at {:.0}% ({}/{})", warning.dimension, warning.usage_pct, warning.used, warning.limit),
                );
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
                    r#"{{"jsonrpc":"2.0","error":{{"code":{},"message":"{safe}"}},"id":null}}"#,
                    error_codes::INTERNAL_ERROR
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

/// Build a JSON-RPC error response with proper Retry-After header.
fn rpc_error_response(
    status: StatusCode,
    code: i32,
    message: &str,
    data_fields: &str,
    retry_after: Option<u64>,
) -> Response {
    let body = format!(
        r#"{{"jsonrpc":"2.0","error":{{"code":{code},"message":"{message}","data":{{{data_fields}}}}},"id":null}}"#,
    );
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(secs) = retry_after {
        builder = builder.header("Retry-After", secs.to_string());
    }
    builder.body(Body::from(body)).unwrap()
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
