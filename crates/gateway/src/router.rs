use std::sync::Arc;

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpStream;
use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use uuid::Uuid;

use crate::{auth, enforce, proxy, user_auth, GateState};

// ---------------------------------------------------------------------------
// Content-Type mapping
// ---------------------------------------------------------------------------

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

/// Known static file extensions that should be served directly (not SPA fallback).
fn is_static_ext(path: &str) -> bool {
    if let Some(ext) = path.rsplit('.').next() {
        matches!(
            ext.to_ascii_lowercase().as_str(),
            "html"
                | "js"
                | "mjs"
                | "css"
                | "json"
                | "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "svg"
                | "ico"
                | "woff"
                | "woff2"
                | "ttf"
                | "webp"
                | "txt"
                | "xml"
                | "webmanifest"
                | "map"
        )
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// App name extraction
// ---------------------------------------------------------------------------

/// Extract the app name from the request.
///
/// 1. If `path_name` is `Some` and non-empty, use it (path-based routing).
/// 2. Otherwise parse the `Host` header: extract `{app_name}.{domain}`.
/// 3. Ignore bare `localhost`, `localhost:PORT`, and IP addresses.
pub fn extract_app_name(req: &HttpRequest, path_name: Option<&str>) -> Option<String> {
    // 1. Path-based routing takes priority
    if let Some(name) = path_name {
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    // 2. Subdomain-based routing via Host header
    let host_header = req.headers().get("host")?.to_str().ok()?;

    // Strip port if present
    let host = host_header.split(':').next().unwrap_or(host_header);

    // Ignore bare localhost
    if host == "localhost" {
        return None;
    }

    // Ignore IP addresses (starts with digit or contains only digits and dots)
    if host.starts_with(|c: char| c.is_ascii_digit())
        && host.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ':')
    {
        return None;
    }

    // IPv6 addresses in brackets
    if host.starts_with('[') {
        return None;
    }

    // Extract first subdomain: "myapp.zeroship.ai" → "myapp"
    // Must have at least one dot (i.e., subdomain.domain)
    let dot_pos = host.find('.')?;
    let subdomain = &host[..dot_pos];

    if subdomain.is_empty() {
        return None;
    }

    Some(subdomain.to_string())
}

// ---------------------------------------------------------------------------
// Fetch asset from control plane
// ---------------------------------------------------------------------------

/// HTTP GET to control plane, returning (status, body_bytes).
async fn fetch_asset(control_url: &str, app_id: &Uuid, path: &str) -> Result<(u16, Vec<u8>), String> {
    let url = format!("{control_url}/internal/assets/{app_id}/{path}");
    let parsed = url::Url::parse(&url).map_err(|e| e.to_string())?;
    let host = parsed.host_str().ok_or("no host")?.to_string();
    let port = parsed.port().unwrap_or(80);
    let req_path = parsed.path();

    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).await.map_err(|e| e.to_string())?;

    let request = format!(
        "GET {req_path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    );

    let BufResult(r, _) = stream.write_all(request.into_bytes()).await;
    r.map_err(|e| e.to_string())?;

    let mut response = Vec::new();
    loop {
        let buf = vec![0u8; 8192];
        let BufResult(r, returned) = stream.read(buf).await;
        let n = r.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        response.extend_from_slice(&returned[..n]);
    }

    let header_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no header end")?;

    let header = std::str::from_utf8(&response[..header_end]).map_err(|e| e.to_string())?;

    // Parse status code from first line
    let status_line = header.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(502);

    let body = response[header_end + 4..].to_vec();
    Ok((status, body))
}

// ---------------------------------------------------------------------------
// Existing path-based handler
// ---------------------------------------------------------------------------

pub async fn handle(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    path: web::types::Path<(String, String)>,
    body: Bytes,
) -> HttpResponse {
    let (app_name, tail) = path.into_inner();
    handle_request(req, state, &app_name, &tail, body).await
}

// ---------------------------------------------------------------------------
// Subdomain-based handler (catch-all)
// ---------------------------------------------------------------------------

pub async fn handle_subdomain(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    path: web::types::Path<String>,
    body: Bytes,
) -> HttpResponse {
    let app_name = match extract_app_name(&req, None) {
        Some(name) => name,
        None => {
            return HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": "could not determine app from Host header"}));
        }
    };
    let tail = path.into_inner();
    handle_request(req, state, &app_name, &tail, body).await
}

// ---------------------------------------------------------------------------
// Unified request handler
// ---------------------------------------------------------------------------

async fn handle_request(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    app_name: &str,
    tail: &str,
    body: Bytes,
) -> HttpResponse {
    let wall_start = std::time::Instant::now();

    // 1. Route resolution — we need the app_id for both RPC and static assets
    let (app_id, route) = match state.routes.lookup_by_name(app_name) {
        Some(r) => r,
        None => {
            return HttpResponse::NotFound()
                .json(&serde_json::json!({"error": format!("app '{app_name}' not found")}));
        }
    };

    // Normalize tail: strip leading slash
    let tail = tail.strip_prefix('/').unwrap_or(tail);

    // 2. Decide: RPC, HTTP dispatch, or static asset
    if tail == "rpc" {
        // RPC request — requires auth, rate limit, proxy to worker
        return handle_rpc(req, &state, &app_id, &route, body, wall_start).await;
    }

    // If the app exports an onRequest handler, proxy non-static HTTP requests
    // to the worker via HTTP dispatch. This enables streaming responses (SSE)
    // and dynamic server-side routing.
    if route.has_http_handler && !is_static_ext(tail) {
        return handle_http_dispatch(req, &state, &app_id, &route, tail, body, wall_start).await;
    }

    // Static file serving — no auth required
    handle_static(&state, &app_id, tail).await
}

// ---------------------------------------------------------------------------
// RPC handler (auth + rate limit + proxy)
// ---------------------------------------------------------------------------

async fn handle_rpc(
    req: HttpRequest,
    state: &GateState,
    app_id: &Uuid,
    route: &zeroship_core::types::RouteEntry,
    body: Bytes,
    wall_start: std::time::Instant,
) -> HttpResponse {
    // Auth — check X-Api-Key header
    if let Err(resp) = auth::check_api_key(&req, route) {
        return resp;
    }

    // Rate limit
    if let Err(resp) = enforce::check_rate_limit(&state.rate_limiters, app_id) {
        return resp;
    }

    // Concurrency guard (RAII — released on drop)
    let _guard = match enforce::acquire_concurrency(&state.concurrency, app_id) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };

    // Extract user from __zs_session cookie (None if missing/invalid/wrong app)
    let user_header_value = if !state.config.auth_secret.is_empty() {
        let cookie = req
            .headers()
            .get("cookie")
            .and_then(|v| v.to_str().ok());
        let app_id_str = app_id.to_string();
        user_auth::extract_user(cookie, &state.config.auth_secret, &app_id_str)
            .map(|u| user_auth::encode_user_header(&u))
    } else {
        None
    };

    // Proxy to worker via CHWBL hash ring
    let request_id = Uuid::new_v4();
    let mut response = match proxy::forward(
        &state.hash_ring,
        app_id,
        &route.plan_id,
        &request_id,
        &body,
        user_header_value.as_deref(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return HttpResponse::BadGateway()
                .json(&serde_json::json!({"error": format!("worker error: {e}")}));
        }
    };

    // Handle 401 response: redirect browser requests to the auth page
    if response.status() == ntex::http::StatusCode::UNAUTHORIZED {
        let accepts_html = req
            .headers()
            .get("accept")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/html"));

        if accepts_html {
            let original_path = req.uri().path();
            let auth_url = format!(
                "{}/auth/authorize?app_id={}&return={}",
                state.config.control_url, app_id, original_path
            );
            return HttpResponse::Found()
                .header("location", auth_url)
                .finish();
        }
    }

    // Add response headers
    let wall_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
    response.headers_mut().insert(
        ntex::http::header::HeaderName::from_static("x-wall-time-ms"),
        ntex::http::header::HeaderValue::from_str(&format!("{wall_ms:.2}")).unwrap(),
    );
    response.headers_mut().insert(
        ntex::http::header::HeaderName::from_static("x-request-id"),
        ntex::http::header::HeaderValue::from_str(&request_id.to_string()).unwrap(),
    );

    response
}

// ---------------------------------------------------------------------------
// HTTP dispatch handler (for apps with onRequest — enables SSE streaming)
// ---------------------------------------------------------------------------

/// Forward an HTTP request to the worker via the `/http-dispatch/` endpoint.
///
/// This path is used for apps that export an `onRequest` handler. The full
/// HTTP request (method, URL, headers, body) is forwarded so the JS handler
/// receives a proper `Request` object and can return streaming responses
/// (e.g., SSE for LLM token streaming).
async fn handle_http_dispatch(
    req: HttpRequest,
    state: &GateState,
    app_id: &Uuid,
    route: &zeroship_core::types::RouteEntry,
    tail: &str,
    body: Bytes,
    wall_start: std::time::Instant,
) -> HttpResponse {
    // Rate limit
    if let Err(resp) = enforce::check_rate_limit(&state.rate_limiters, app_id) {
        return resp;
    }

    // Concurrency guard (RAII — released on drop)
    let _guard = match enforce::acquire_concurrency(&state.concurrency, app_id) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };

    // Extract user from __zs_session cookie
    let user_header_value = if !state.config.auth_secret.is_empty() {
        let cookie = req
            .headers()
            .get("cookie")
            .and_then(|v| v.to_str().ok());
        let app_id_str = app_id.to_string();
        user_auth::extract_user(cookie, &state.config.auth_secret, &app_id_str)
            .map(|u| user_auth::encode_user_header(&u))
    } else {
        None
    };

    // Reconstruct the URL the JS handler will see
    let scheme = if req.connection_info().scheme() == "https" { "https" } else { "http" };
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let url = format!("{scheme}://{host}/{tail}");

    // Collect request headers as [key, value] pairs
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in req.headers() {
        if let Ok(v) = value.to_str() {
            headers.push((name.as_str().to_string(), v.to_string()));
        }
    }

    let method = req.method().as_str();
    let body_str = String::from_utf8_lossy(&body);

    // Proxy to worker via CHWBL hash ring using HTTP dispatch
    let request_id = Uuid::new_v4();
    let mut response = match proxy::forward_http(
        &state.hash_ring,
        app_id,
        &route.plan_id,
        &request_id,
        method,
        &url,
        &headers,
        &body_str,
        user_header_value.as_deref(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return HttpResponse::BadGateway()
                .json(&serde_json::json!({"error": format!("worker error: {e}")}));
        }
    };

    // Add response headers
    let wall_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
    response.headers_mut().insert(
        ntex::http::header::HeaderName::from_static("x-wall-time-ms"),
        ntex::http::header::HeaderValue::from_str(&format!("{wall_ms:.2}")).unwrap(),
    );
    response.headers_mut().insert(
        ntex::http::header::HeaderName::from_static("x-request-id"),
        ntex::http::header::HeaderValue::from_str(&request_id.to_string()).unwrap(),
    );

    response
}

// ---------------------------------------------------------------------------
// Static file handler
// ---------------------------------------------------------------------------

async fn handle_static(
    state: &GateState,
    app_id: &Uuid,
    tail: &str,
) -> HttpResponse {
    // Determine the asset path to fetch
    let asset_path = if tail.is_empty() || tail == "index.html" {
        "index.html"
    } else {
        tail
    };

    // Fetch from control plane
    match fetch_asset(&state.config.control_url, app_id, asset_path).await {
        Ok((200, data)) => {
            let ct = content_type_for_ext(asset_path);
            HttpResponse::Ok()
                .content_type(ct)
                .body(data)
        }
        Ok((404, _)) => {
            // If it's a known static extension, return 404
            if is_static_ext(asset_path) {
                return HttpResponse::NotFound()
                    .json(&serde_json::json!({"error": "asset not found"}));
            }
            // SPA fallback: try index.html for unknown paths
            match fetch_asset(&state.config.control_url, app_id, "index.html").await {
                Ok((200, data)) => {
                    HttpResponse::Ok()
                        .content_type("text/html; charset=utf-8")
                        .body(data)
                }
                Ok(_) => {
                    HttpResponse::NotFound()
                        .json(&serde_json::json!({"error": "index.html not found"}))
                }
                Err(_) => {
                    HttpResponse::ServiceUnavailable()
                        .json(&serde_json::json!({"error": "control plane unavailable"}))
                }
            }
        }
        Ok((status, body)) => {
            HttpResponse::build(
                ntex::http::StatusCode::from_u16(status)
                    .unwrap_or(ntex::http::StatusCode::INTERNAL_SERVER_ERROR),
            )
            .body(body)
        }
        Err(_) => {
            HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": "control plane unavailable"}))
        }
    }
}
