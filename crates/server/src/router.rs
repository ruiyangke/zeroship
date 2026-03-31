//! Axum router for appbase — RPC dispatch with metering, quota enforcement, and admin API.

use appbase_core::config::AppbaseConfig;
use appbase_core::plugin::{MeterFactory, PluginFactory};
use appbase_core::types::AppBundle;
use appbase_isolate::pool::IsolatePool;
use appbase_core::event_log::EventKind;
use appbase_metering::concurrency::ConcurrencyGuard;
use appbase_billing::spend_action::SpendAction;
use appbase_metering::enforcer::{self, QuotaDecision};
use appbase_metering::error_codes;
use appbase_metering::event_channel::EventSender;
use appbase_metering::meter::MeterRegistry;
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

/// Per-app concurrency gauges — keyed by app_id.
pub struct ConcurrencyRegistry {
    gauges: std::sync::RwLock<HashMap<String, Arc<AtomicU32>>>,
}

impl ConcurrencyRegistry {
    pub fn new() -> Self {
        Self {
            gauges: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Get or create a per-app concurrency gauge.
    pub fn get_or_create(&self, app_id: &str) -> Arc<AtomicU32> {
        // Fast path: read lock
        {
            let gauges = self.gauges.read().unwrap();
            if let Some(gauge) = gauges.get(app_id) {
                return gauge.clone();
            }
        }
        // Slow path: write lock
        let mut gauges = self.gauges.write().unwrap();
        gauges
            .entry(app_id.to_string())
            .or_insert_with(|| Arc::new(AtomicU32::new(0)))
            .clone()
    }

    /// Get current in-flight count for an app.
    pub fn current(&self, app_id: &str) -> u32 {
        let gauges = self.gauges.read().unwrap();
        gauges
            .get(app_id)
            .map(|g| g.load(Ordering::Acquire))
            .unwrap_or(0)
    }
}

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
    /// Per-app concurrency gauges.
    pub concurrency: Arc<ConcurrencyRegistry>,
    /// Event sender for cold-tier event logging.
    pub event_sender: EventSender,
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
    event_sender: EventSender,
) -> AppState {
    let default_plan = plan.unwrap_or_else(QuotaPlan::unlimited);

    // Collect plugin-declared meter resources by creating a temporary set of plugins.
    // This discovers resources like db.reads, kv.writes, etc. that plugins track.
    let sample_plugins = plugin_factory("_resource_discovery");
    let plugin_resources: Vec<appbase_core::plugin::MeterResource> = sample_plugins
        .iter()
        .flat_map(|p| p.meter_resources())
        .collect();
    let meters = Arc::new(MeterRegistry::new(default_plan, plugin_resources));

    // Create a meter factory that produces AppPluginMeter instances backed by the
    // shared MeterRegistry. This ensures plugin ops (db.reads, kv.writes, etc.)
    // are recorded against the correct app's meter instead of being discarded.
    let meters_for_factory = meters.clone();
    let meter_factory: MeterFactory = Arc::new(move |app_id: &str| -> Arc<dyn appbase_core::plugin::PluginMeter> {
        Arc::new(appbase_metering::meter::AppPluginMeter::new(
            meters_for_factory.clone(),
            app_id.to_string(),
        ))
    });

    let pool = IsolatePool::new(config.isolates.clone(), data_dir, plugin_factory, meter_factory);

    let mut bundles = HashMap::new();
    bundles.insert(
        "default".to_string(),
        AppBundle {
            server_js,
            client_html: client_html.clone(),
        },
    );

    let rate_limiter = Arc::new(RateLimiter::new(10000, 50000));

    AppState {
        pool,
        bundles: Arc::new(Mutex::new(bundles)),
        default_app: "default".to_string(),
        static_html: client_html.map(Bytes::from),
        meters,
        rate_limiter,
        concurrency: Arc::new(ConcurrencyRegistry::new()),
        event_sender,
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
        state.event_sender.log_enforcement(&app_id, EventKind::RateLimited, serde_json::json!({}));
        return rpc_error_response(
            StatusCode::TOO_MANY_REQUESTS,
            error_codes::RATE_LIMITED,
            "Rate limit exceeded",
            r#""type":"rate_limited""#,
            Some(1), // retry after 1 second for rate limit
        );
    }

    // 2. Concurrency guard — RAII, auto-decrements on drop
    let app_gauge = state.concurrency.get_or_create(&app_id);
    let _concurrency_guard = match ConcurrencyGuard::try_acquire(
        &app_gauge,
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

    // Entitlement checks are NOT performed on every RPC call. Per the spec,
    // entitlements gate specific features (custom_domains, cron_jobs, websockets),
    // not general RPC access. Feature-specific entitlement checks should be added
    // at the routing layer for those features (e.g., WebSocket upgrade handler,
    // cron job creation endpoint). See enforcer::check_entitlement().

    // 3. Spending limit check
    let meter = state.meters.get_or_create(&app_id);
    let spend_action = SpendAction::load(&meter.spend_action);
    match spend_action {
        SpendAction::Block => {
            let reset = seconds_until_period_reset();
            return rpc_error_response(
                StatusCode::TOO_MANY_REQUESTS,
                error_codes::SPENDING_LIMIT,
                "Spending limit reached",
                r#""type":"spending_limit""#,
                Some(reset),
            );
        }
        SpendAction::Warn | SpendAction::Degrade | SpendAction::Allow => {
            // Warn/Degrade handled below in response headers
        }
    }

    // 4. Quota check — single call, capture both deny and warnings
    let quota_decision = enforcer::check_quota(&meter, &meter.plan);
    let quota_warnings = match &quota_decision {
        QuotaDecision::Deny(denial) => {
            state.event_sender.log_enforcement(&app_id, EventKind::QuotaDenied, serde_json::json!({
                "dimension": denial.dimension,
            }));
            let reset = seconds_until_period_reset();
            return rpc_error_response(
                StatusCode::TOO_MANY_REQUESTS,
                denial.error_code,
                &denial.message,
                &format!(
                    r#""type":"quota_exceeded","dimension":"{}","used":{},"limit":{}"#,
                    json_escape(&denial.dimension), denial.used, denial.limit
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
    let ingress_bytes = body.len() as u64;
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
            meter.record_request(
                rpc_result.cpu_time.as_micros() as u64,
                wall_time.as_micros() as u64,
                response_bytes,
                ingress_bytes,
            );

            // Enqueue event for cold tier
            state.event_sender.log_request(&app_id, serde_json::json!({
                "cpu_ms": cpu_ms,
                "wall_ms": wall_time.as_secs_f64() * 1000.0,
                "egress_bytes": response_bytes,
            }));

            // 8. Build response with metering + IETF RateLimit headers
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
                    let used = meter.counters.load(meter.core.requests);
                    let remaining = limit.saturating_sub(used);
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

            // Spending warning header
            if spend_action == SpendAction::Warn {
                add_header(&mut headers, "x-spending-warning", "Approaching spending limit");
            }

            // Quota warning headers (spec §8.2) — use append, not insert, for multi-value
            for warning in &quota_warnings {
                append_header(
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
            let safe = json_escape(&sanitize_error(&e));
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

/// GET /_usage — all apps' usage with concurrency info.
async fn handle_all_usage(State(state): State<AppState>) -> Response {
    let usage = state.meters.all_usage();
    // Enrich each app's snapshot with concurrent_requests
    let mut enriched = serde_json::Map::new();
    for (app_id, counters) in &usage {
        let mut obj = serde_json::Map::new();
        for (key, &val) in counters {
            obj.insert(key.clone(), serde_json::Value::from(val));
        }
        obj.insert(
            "concurrent_requests".to_string(),
            serde_json::Value::from(state.concurrency.current(app_id)),
        );
        enriched.insert(app_id.clone(), serde_json::Value::Object(obj));
    }
    json_response(
        StatusCode::OK,
        &serde_json::Value::Object(enriched).to_string(),
    )
}

/// GET /_apps/{app_id}/usage — single app's usage + concurrency.
async fn handle_app_usage(
    State(state): State<AppState>,
    Path(app_id): Path<String>,
) -> Response {
    match state.meters.get_usage(&app_id) {
        Some(counters) => {
            let mut obj = serde_json::Map::new();
            for (key, &val) in &counters {
                obj.insert(key.clone(), serde_json::Value::from(val));
            }
            obj.insert(
                "concurrent_requests".to_string(),
                serde_json::Value::from(state.concurrency.current(&app_id)),
            );
            json_response(StatusCode::OK, &serde_json::Value::Object(obj).to_string())
        }
        None => json_response(
            StatusCode::NOT_FOUND,
            &format!(r#"{{"error":"No usage data for '{}'"}}"#, json_escape(&app_id)),
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
        &format!(r#"{{"evicted":"{}"}}"#, json_escape(&app_id)),
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

/// Escape a string for safe interpolation into a JSON string literal.
/// Handles backslash, double-quote, newline, and carriage return.
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

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
    let safe_message = json_escape(message);
    let body = format!(
        r#"{{"jsonrpc":"2.0","error":{{"code":{code},"message":"{safe_message}","data":{{{data_fields}}}}},"id":null}}"#,
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

/// Append a header value (allows multiple values for the same header name).
fn append_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    let n = axum::http::header::HeaderName::from_static(name);
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.append(n, v);
    }
}

fn sanitize_error(msg: &str) -> String {
    msg.lines()
        .next()
        .unwrap_or("Internal error")
        .replace('/', "")
        .replace('\\', "")
}
