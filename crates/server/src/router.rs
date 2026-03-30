//! Axum router for appbase — RPC dispatch, static serving, health/stats endpoints.

use appbase_core::config::AppbaseConfig;
use appbase_core::plugin::PluginFactory;
use appbase_core::types::AppBundle;
use appbase_isolate::pool::IsolatePool;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
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
    /// Isolate pool — manages V8 workers, one per app.
    pub pool: Arc<IsolatePool>,
    /// App bundles keyed by app ID.
    pub bundles: Arc<Mutex<HashMap<String, AppBundle>>>,
    /// Default app ID for single-app mode.
    pub default_app: String,
    /// Pre-loaded static HTML (zero-copy via Bytes).
    pub static_html: Option<Bytes>,
}

/// Build the axum router with all routes and middleware.
pub fn build(state: AppState) -> Router {
    let (cors, _compression) = middleware::production_layers();

    Router::new()
        .route("/rpc", post(handle_rpc))
        .route("/_stats", get(handle_stats))
        .route("/_health", get(handle_health))
        .route("/_apps/{app_id}", delete(handle_evict_app))
        .fallback(get(handle_static))
        .layer(cors)
        .with_state(state)
}

/// Create an AppState for single-app mode.
pub fn single_app_state(
    server_js: String,
    client_html: Option<Vec<u8>>,
    config: &AppbaseConfig,
    data_dir: PathBuf,
    plugin_factory: PluginFactory,
) -> AppState {
    let pool = IsolatePool::new(
        config.isolates.clone(),
        data_dir,
        plugin_factory,
    );

    let mut bundles = HashMap::new();
    bundles.insert(
        "default".to_string(),
        AppBundle {
            server_js,
            client_html: client_html.clone(),
        },
    );

    AppState {
        pool,
        bundles: Arc::new(Mutex::new(bundles)),
        default_app: "default".to_string(),
        static_html: client_html.map(Bytes::from),
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

/// POST /rpc — dispatch JSON-RPC to the app's V8 isolate.
async fn handle_rpc(State(state): State<AppState>, body: String) -> Response {
    // TODO: extract app_id from header/subdomain for multi-tenant
    let app_id = state.default_app.clone();

    // Clone bundle and drop lock before any .await
    let bundle = {
        let bundles = state.bundles.lock().unwrap();
        match bundles.get(&app_id) {
            Some(b) => b.clone(),
            None => return json_response(StatusCode::NOT_FOUND, r#"{"error":"App not found"}"#),
        }
    };

    match state.pool.dispatch(&app_id, &bundle.server_js, body).await {
        Ok(result) => {
            tracing::info!(
                app = app_id,
                cpu_ms = format!("{:.2}", result.cpu_time.as_secs_f64() * 1000.0),
                "RPC"
            );
            json_response(StatusCode::OK, &result.json)
        }
        Err(e) => {
            let safe = sanitize_error(&e);
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!(r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"{safe}"}},"id":null}}"#),
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

/// DELETE /_apps/{app_id} — manually evict an app's isolate.
async fn handle_evict_app(
    State(state): State<AppState>,
    Path(app_id): Path<String>,
) -> Response {
    if state.pool.evict_app(&app_id) {
        json_response(StatusCode::OK, &format!(r#"{{"evicted":"{app_id}"}}"#))
    } else {
        json_response(StatusCode::NOT_FOUND, &format!(r#"{{"error":"App '{app_id}' not found"}}"#))
    }
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

fn sanitize_error(msg: &str) -> String {
    msg.lines()
        .next()
        .unwrap_or("Internal error")
        .replace('/', "")
        .replace('\\', "")
}
