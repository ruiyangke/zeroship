//! Axum router for appbase — RPC dispatch with metering, quota enforcement, and admin API.

use crate::control::AppRegistry;
use crate::core::config::AppbaseConfig;
use crate::core::plugin::PluginFactory;
use crate::core::types::AppBundle;
use crate::core::event_log::EventKind;
use crate::server::v8pool::{PoolDispatchResult, V8Pool};
use crate::enforcement::concurrency::ConcurrencyGuard;
use crate::enforcement::quota;
use crate::enforcement::error_codes;
use crate::enforcement::rate_limit::RateLimiter;
use crate::core::billing::SpendAction;
use crate::metering::event_channel::EventSender;
use crate::metering::meter::MeterRegistry;
use crate::plan::QuotaPlan;
use std::sync::atomic::AtomicU32;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use futures::StreamExt as _;
use tokio_stream::wrappers::ReceiverStream;
use axum::routing::{delete, get, post, put};
use axum::Router;
use bytes::Bytes;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};

use crate::server::middleware;

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
    pub pool: Arc<V8Pool>,
    pub registry: Arc<dyn AppRegistry>,
    pub bundles: Arc<Mutex<HashMap<String, AppBundle>>>,
    pub bundle_versions: Arc<RwLock<HashMap<String, i64>>>,
    pub default_app: String,
    pub static_html: Option<Bytes>,
    /// Master key for admin API endpoints.
    pub master_key: String,
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
    let (cors, compression) = middleware::production_layers();

    Router::new()
        .route("/rpc", post(handle_rpc))
        .route("/_stats", get(handle_stats))
        .route("/_health", get(handle_health))
        .route("/_apps/{app_id}", delete(handle_evict_app))
        .route("/_apps/{app_id}/usage", get(handle_app_usage))
        .route("/_usage", get(handle_all_usage))
        // Control plane API
        .route("/api/apps", post(handle_create_app))
        .route("/api/apps", get(handle_list_apps))
        .route("/api/apps/{id}", get(handle_get_app))
        .route("/api/apps/{id}", delete(handle_delete_app_api))
        .route("/api/apps/{id}/deploy", post(handle_deploy))
        .route("/api/apps/{id}/plan", put(handle_set_plan))
        .route("/api/apps/{id}/logs", get(handle_app_logs))
        .route("/api/templates", get(handle_templates))
        .fallback(get(handle_static))
        .layer(cors)
        .layer(compression)
        .with_state(state)
}

/// Create an AppState for single-app mode (one app per server instance).
///
/// If `plan` is provided, it is used for metering; otherwise defaults to unlimited (dev-friendly).
pub fn single_app_state(
    server_js: String,
    client_html: Option<Vec<u8>>,
    config: &AppbaseConfig,
    _data_dir: PathBuf,
    plugin_factory: PluginFactory,
    plan: Option<QuotaPlan>,
    event_sender: EventSender,
    registry: Arc<dyn AppRegistry>,
    master_key: String,
) -> AppState {
    let default_plan = plan.unwrap_or_else(QuotaPlan::unlimited);

    // Collect plugin-declared meter resources by creating a temporary set of plugins.
    // This discovers resources like db.reads, kv.writes, etc. that plugins track.
    let sample_plugins = plugin_factory("_resource_discovery");
    let plugin_resources: Vec<crate::core::plugin::MeterResource> = sample_plugins
        .iter()
        .flat_map(|p| p.meter_resources())
        .collect();
    let meters = Arc::new(MeterRegistry::new(default_plan, plugin_resources));

    // Note: plugin-level metering/quota inside isolates is not yet wired into
    // V8Pool. It will be added when plugin support lands.

    let pool = Arc::new(V8Pool::new(&config.isolates));

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
        registry,
        bundles: Arc::new(Mutex::new(bundles)),
        bundle_versions: Arc::new(RwLock::new(HashMap::new())),
        default_app: "default".to_string(),
        static_html: client_html.map(Bytes::from),
        master_key,
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

    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            eprintln!("[appbase] Shutting down gracefully...");
        });

    server.await.map_err(|e| format!("Server error: {e}"))
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
async fn handle_rpc(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // Extract app_id: subdomain > X-App-Id header > default
    let app_id = extract_app_id(&headers, &state.default_app);

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

    // 4. Pre-dispatch quota check for request-level dimensions (requests, cpu_us).
    //    Plugin-level quotas (db.reads, kv.writes) are checked at point of use.
    let usage = meter.counters.snapshot();
    match quota::check_quota(&usage, &meter.plan) {
        quota::QuotaDecision::Deny(denial) => {
            state.event_sender.log_enforcement(&app_id, EventKind::QuotaDenied, serde_json::json!({
                "dimension": denial.dimension,
            }));
            let reset = seconds_until_period_reset();
            return rpc_error_response(
                StatusCode::TOO_MANY_REQUESTS,
                denial.error_code,
                &json_escape(&denial.message),
                &format!(
                    r#""type":"quota_exceeded","dimension":"{}","used":{},"limit":{}"#,
                    json_escape(&denial.dimension), denial.used, denial.limit
                ),
                Some(reset),
            );
        }
        quota::QuotaDecision::Warn(_) | quota::QuotaDecision::Allow => {}
    }

    // 5. Load app bundle (cache → registry)
    let cached = {
        let bundles = state.bundles.lock().unwrap();
        bundles.get(&app_id).cloned()
    };
    let bundle = if let Some(b) = cached {
        b
    } else {
        // Cache miss → load from registry
        match state.registry.get_app(&app_id).await {
            Ok(Some(data)) => {
                let bundle = AppBundle {
                    server_js: data.server_js,
                    client_html: data.client_html,
                };
                state.bundles.lock().unwrap().insert(app_id.clone(), bundle.clone());
                state.bundle_versions.write().unwrap().insert(app_id.clone(), data.version);
                bundle
            }
            Ok(None) => return json_response(StatusCode::NOT_FOUND, r#"{"error":"App not found"}"#),
            Err(e) => return json_response(StatusCode::INTERNAL_SERVER_ERROR, &format!(r#"{{"error":"{}"}}"#, e)),
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
        Ok(PoolDispatchResult::Stream(stream)) => {
            // Streaming response — forward status, headers, and body channel directly.
            let body_stream = ReceiverStream::new(stream.body_rx)
                .map(|chunk| Ok::<_, std::io::Error>(chunk));
            let body = Body::from_stream(body_stream);
            let mut builder = axum::http::Response::builder()
                .status(stream.status);
            for (k, v) in &stream.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            builder.body(body).unwrap_or_else(|_| {
                axum::http::Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())
                    .unwrap()
            })
        }
        Ok(PoolDispatchResult::Rpc(rpc_result)) => {
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

            // Quota warning headers (spec §8.2) — computed post-dispatch from snapshot
            let usage_snapshot = meter.counters.snapshot();
            for (resource, quota) in &meter.plan.quotas {
                if let Some(max) = quota.max {
                    let used = usage_snapshot.get(resource).copied().unwrap_or(0);
                    if max > 0 {
                        let pct = used as f64 / max as f64 * 100.0;
                        if pct >= 80.0 {
                            append_header(
                                &mut headers,
                                "x-quota-warning",
                                &format!("{resource} at {pct:.0}% ({used}/{max})"),
                            );
                        }
                    }
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

/// GET /_stats — pool statistics (master key required).
async fn handle_stats(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !check_master_key(&state, &headers) {
        return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"Invalid or missing master key"}"#);
    }
    let stats = state.pool.stats();
    json_response(
        StatusCode::OK,
        &serde_json::to_string(&stats).unwrap_or_default(),
    )
}

/// GET /_usage — all apps' usage with concurrency info (master key required).
async fn handle_all_usage(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !check_master_key(&state, &headers) {
        return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"Invalid or missing master key"}"#);
    }
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

/// GET /_apps/{app_id}/usage — single app's usage + concurrency (master key required).
async fn handle_app_usage(
    State(state): State<AppState>,
    Path(app_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !check_master_key(&state, &headers) {
        return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"Invalid or missing master key"}"#);
    }
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

/// DELETE /_apps/{app_id} — manually evict + clear meter (master key required).
async fn handle_evict_app(
    State(state): State<AppState>,
    Path(app_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !check_master_key(&state, &headers) {
        return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"Invalid or missing master key"}"#);
    }
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

// --- App ID resolution ---

/// Validate an app_id: non-empty, max 64 chars, only alphanumeric + hyphens + underscores.
fn is_valid_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Three-level app ID resolution: subdomain > X-App-Id header > default.
fn extract_app_id(headers: &HeaderMap, default: &str) -> String {
    // 1. Check Host header for subdomain: my-app.platform.dev → "my-app"
    if let Some(host) = headers.get("host").and_then(|v| v.to_str().ok()) {
        let parts: Vec<&str> = host.split('.').collect();
        if parts.len() >= 3 {
            let subdomain = parts[0];
            if subdomain != "www" && subdomain != "api" && is_valid_app_id(subdomain) {
                return subdomain.to_string();
            }
        }
    }
    // 2. Check X-App-Id header
    if let Some(id) = headers.get("x-app-id").and_then(|v| v.to_str().ok()) {
        if is_valid_app_id(id) {
            return id.to_string();
        }
    }
    // 3. Default app (also used when validation fails — don't error on the data plane)
    default.to_string()
}

// --- Auth helpers ---

fn extract_bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Constant-time string comparison to prevent timing attacks on secrets.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn check_master_key(state: &AppState, headers: &HeaderMap) -> bool {
    extract_bearer(headers)
        .map(|k| constant_time_eq(k, &state.master_key))
        .unwrap_or(false)
}

// --- Control plane handlers ---

/// POST /api/apps — create a new app (master key required).
async fn handle_create_app(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !check_master_key(&state, &headers) {
        return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"Invalid or missing master key"}"#);
    }

    #[derive(serde::Deserialize)]
    struct CreateBody {
        id: String,
        #[serde(default = "default_plan_id")]
        plan_id: String,
    }
    fn default_plan_id() -> String {
        "free".to_string()
    }

    let parsed: CreateBody = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &format!(r#"{{"error":"Invalid JSON: {}"}}"#, json_escape(&e.to_string())),
            );
        }
    };

    match state.registry.create_app(&parsed.id, &parsed.plan_id).await {
        Ok(record) => json_response(
            StatusCode::CREATED,
            &serde_json::to_string(&record).unwrap_or_default(),
        ),
        Err(crate::control::RegistryError::AlreadyExists(_)) => {
            json_response(StatusCode::CONFLICT, r#"{"error":"App already exists"}"#)
        }
        Err(crate::control::RegistryError::InvalidInput(msg)) => {
            json_response(StatusCode::BAD_REQUEST, &format!(r#"{{"error":"{}"}}"#, json_escape(&msg)))
        }
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!(r#"{{"error":"{}"}}"#, json_escape(&e.to_string())),
        ),
    }
}

/// GET /api/apps — list all apps (master key required).
async fn handle_list_apps(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if !check_master_key(&state, &headers) {
        return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"Invalid or missing master key"}"#);
    }

    match state.registry.list_apps().await {
        Ok(apps) => json_response(
            StatusCode::OK,
            &serde_json::to_string(&apps).unwrap_or_default(),
        ),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!(r#"{{"error":"{}"}}"#, json_escape(&e.to_string())),
        ),
    }
}

/// GET /api/apps/:id — get app info (master key required).
async fn handle_get_app(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !check_master_key(&state, &headers) {
        return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"Invalid or missing master key"}"#);
    }

    match state.registry.get_app(&id).await {
        Ok(Some(data)) => {
            let info = serde_json::json!({
                "id": data.id,
                "plan_id": data.plan_id,
                "version": data.version,
                "server_js": data.server_js,
                "api_key": data.api_key,
                "created_at": data.created_at,
                "updated_at": data.updated_at,
            });
            json_response(StatusCode::OK, &info.to_string())
        }
        Ok(None) => json_response(StatusCode::NOT_FOUND, r#"{"error":"App not found"}"#),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!(r#"{{"error":"{}"}}"#, json_escape(&e.to_string())),
        ),
    }
}

/// DELETE /api/apps/:id — delete an app (master key required).
async fn handle_delete_app_api(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !check_master_key(&state, &headers) {
        return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"Invalid or missing master key"}"#);
    }

    match state.registry.delete_app(&id).await {
        Ok(true) => {
            // Evict from cache and pool
            state.bundles.lock().unwrap().remove(&id);
            state.bundle_versions.write().unwrap().remove(&id);
            state.pool.evict_app(&id);
            state.meters.remove(&id);
            state.rate_limiter.remove(&id);
            json_response(StatusCode::OK, &format!(r#"{{"deleted":"{}"}}"#, json_escape(&id)))
        }
        Ok(false) => json_response(StatusCode::NOT_FOUND, r#"{"error":"App not found"}"#),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!(r#"{{"error":"{}"}}"#, json_escape(&e.to_string())),
        ),
    }
}

/// POST /api/apps/:id/deploy — deploy JS to an app (per-app API key required).
async fn handle_deploy(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // Auth: per-app key or master key
    let key = match extract_bearer(&headers) {
        Some(k) => k.to_string(),
        None => {
            return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"Missing Authorization header"}"#);
        }
    };

    let is_master = constant_time_eq(&key, &state.master_key);
    if !is_master {
        match state.registry.validate_key(&id, &key).await {
            Ok(true) => {}
            Ok(false) => {
                return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"Invalid API key"}"#);
            }
            Err(e) => {
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!(r#"{{"error":"{}"}}"#, json_escape(&e.to_string())),
                );
            }
        }
    }

    if body.is_empty() {
        return json_response(StatusCode::BAD_REQUEST, r#"{"error":"Empty deploy body"}"#);
    }

    match state.registry.deploy(&id, &body, None).await {
        Ok(version) => {
            // Evict cached bundle and isolate so next request picks up new code
            state.bundles.lock().unwrap().remove(&id);
            state.bundle_versions.write().unwrap().insert(id.clone(), version);
            state.pool.evict_app(&id);
            let test_curl = format!(
                "curl -X POST http://localhost:3333/rpc -H 'X-App-Id: {id}' -H 'Content-Type: application/json' -d '{{\"jsonrpc\":\"2.0\",\"method\":\"YOUR_METHOD\",\"params\":[],\"id\":1}}'"
            );
            let resp = serde_json::json!({
                "version": version,
                "app_id": id,
                "test": test_curl,
            });
            json_response(StatusCode::OK, &resp.to_string())
        }
        Err(crate::control::RegistryError::NotFound(_)) => {
            json_response(StatusCode::NOT_FOUND, r#"{"error":"App not found"}"#)
        }
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!(r#"{{"error":"{}"}}"#, json_escape(&e.to_string())),
        ),
    }
}

/// PUT /api/apps/:id/plan — set an app's plan (master key required).
async fn handle_set_plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !check_master_key(&state, &headers) {
        return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"Invalid or missing master key"}"#);
    }

    #[derive(serde::Deserialize)]
    struct PlanBody {
        plan_id: String,
    }

    let parsed: PlanBody = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &format!(r#"{{"error":"Invalid JSON: {}"}}"#, json_escape(&e.to_string())),
            );
        }
    };

    match state.registry.set_plan(&id, &parsed.plan_id).await {
        Ok(true) => json_response(StatusCode::OK, &format!(r#"{{"plan_id":"{}"}}"#, json_escape(&parsed.plan_id))),
        Ok(false) => json_response(StatusCode::NOT_FOUND, r#"{"error":"App not found"}"#),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!(r#"{{"error":"{}"}}"#, json_escape(&e.to_string())),
        ),
    }
}

/// GET /api/apps/:id/logs — recent console output for an app.
async fn handle_app_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !check_master_key(&state, &headers) {
        return json_response(StatusCode::UNAUTHORIZED, r#"{"error":"Invalid or missing master key"}"#);
    }

    let logs = state.pool.get_logs(&id);
    json_response(StatusCode::OK, &serde_json::to_string(&logs).unwrap_or_default())
}

/// GET /api/templates — list available starter templates.
async fn handle_templates() -> Response {
    let templates = serde_json::json!([
        {
            "id": "hello-world",
            "name": "Hello World",
            "description": "Simple ping/pong API",
            "code": "export function ping() {\n  return \"pong\";\n}\n\nexport function hello(name) {\n  return \"Hello, \" + (name || \"world\") + \"!\";\n}"
        },
        {
            "id": "todo-api",
            "name": "Todo API",
            "description": "In-memory todo list with CRUD operations",
            "code": "let todos = [];\nlet nextId = 1;\n\nexport function list() {\n  return todos;\n}\n\nexport function add(title) {\n  const todo = { id: nextId++, title: title, done: false };\n  todos.push(todo);\n  return todo;\n}\n\nexport function toggle(id) {\n  const todo = todos.find(t => t.id === id);\n  if (!todo) return { error: \"not found\" };\n  todo.done = !todo.done;\n  return todo;\n}\n\nexport function remove(id) {\n  const idx = todos.findIndex(t => t.id === id);\n  if (idx === -1) return { error: \"not found\" };\n  return todos.splice(idx, 1)[0];\n}"
        },
        {
            "id": "weather-proxy",
            "name": "Weather Proxy",
            "description": "Fetch weather data from wttr.in",
            "code": "export async function get(city) {\n  const resp = await fetch(\"https://wttr.in/\" + (city || \"London\") + \"?format=j1\");\n  const data = await resp.json();\n  const current = data.current_condition[0];\n  return {\n    city: city || \"London\",\n    temp_c: current.temp_C,\n    feels_like_c: current.FeelsLikeC,\n    description: current.weatherDesc[0].value,\n    humidity: current.humidity\n  };\n}"
        },
        {
            "id": "math-api",
            "name": "Math API",
            "description": "Basic math operations",
            "code": "export function add(a, b) { return a + b; }\nexport function subtract(a, b) { return a - b; }\nexport function multiply(a, b) { return a * b; }\n\nexport function divide(a, b) {\n  if (b === 0) throw new Error(\"Division by zero\");\n  return a / b;\n}\n\nexport function factorial(n) {\n  if (n < 0) throw new Error(\"Negative input\");\n  if (n <= 1) return 1;\n  let result = 1;\n  for (let i = 2; i <= n; i++) result *= i;\n  return result;\n}"
        }
    ]);
    json_response(StatusCode::OK, &templates.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_app_id_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-app-id", "my-app".parse().unwrap());
        assert_eq!(extract_app_id(&headers, "default"), "my-app");
    }

    #[test]
    fn test_extract_app_id_from_subdomain() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "my-app.platform.dev".parse().unwrap());
        assert_eq!(extract_app_id(&headers, "default"), "my-app");
    }

    #[test]
    fn test_extract_app_id_default() {
        let headers = HeaderMap::new();
        assert_eq!(extract_app_id(&headers, "default"), "default");
    }

    #[test]
    fn test_extract_app_id_www_subdomain_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "www.platform.dev".parse().unwrap());
        assert_eq!(extract_app_id(&headers, "default"), "default");
    }

    #[test]
    fn test_extract_app_id_invalid_chars_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert("x-app-id", "app with spaces".parse().unwrap());
        assert_eq!(extract_app_id(&headers, "default"), "default");
    }

    #[test]
    fn test_extract_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer my-key".parse().unwrap());
        assert_eq!(extract_bearer(&headers), Some("my-key"));
    }

    #[test]
    fn test_extract_bearer_missing() {
        let headers = HeaderMap::new();
        assert_eq!(extract_bearer(&headers), None);
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn test_is_valid_app_id() {
        assert!(is_valid_app_id("my-app"));
        assert!(is_valid_app_id("app_123"));
        assert!(is_valid_app_id("a"));
        assert!(!is_valid_app_id(""));
        assert!(!is_valid_app_id("app with spaces"));
        assert!(!is_valid_app_id("app/path"));
        assert!(!is_valid_app_id(&"a".repeat(65)));
    }

    #[test]
    fn test_json_escape() {
        assert_eq!(json_escape("hello"), "hello");
        assert_eq!(json_escape("he\"llo"), "he\\\"llo");
        assert_eq!(json_escape("line\nbreak"), "line\\nbreak");
    }
}
