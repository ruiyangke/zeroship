use std::sync::Arc;

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpStream;
use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use uuid::Uuid;

use crate::{auth, enforce, proxy, user_auth, GateState};

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

    // 1. Route resolution — we need the app_id for both dispatch and static assets.
    let (app_id, compiled_route) = match state.routes.lookup_by_name(app_name) {
        Some(r) => r,
        None => {
            return HttpResponse::NotFound()
                .json(&serde_json::json!({"error": format!("app '{app_name}' not found")}));
        }
    };

    // Normalize tail: strip leading slash
    let tail = tail.strip_prefix('/').unwrap_or(tail);

    // Manifest-driven dispatch. Every app has a manifest (synthesized
    // passthrough for apps that haven't declared one), so dispatch is
    // always defined. The pre-compiled form does the work — no per-request
    // HashMap allocation, no per-call String::replace.
    let dispatch_path = format!("/{tail}");
    let outcome = compiled_route
        .manifest
        .dispatch(req.method().as_str(), &dispatch_path);
    execute_outcome(
        outcome,
        req,
        state,
        &app_id,
        &compiled_route.entry,
        tail,
        body,
        wall_start,
    )
    .await
}

// ---------------------------------------------------------------------------
// Manifest-driven outcome execution
// ---------------------------------------------------------------------------

/// Execute the [`Outcome`] produced by the manifest's rule walk.
async fn execute_outcome(
    outcome: crate::dispatch::Outcome,
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    app_id: &Uuid,
    route: &zeroship_core::types::RouteEntry,
    tail: &str,
    body: Bytes,
    wall_start: std::time::Instant,
) -> HttpResponse {
    use crate::dispatch::Outcome;
    use zeroship_core::types::WorkerMode;

    match outcome {
        Outcome::Worker { mode, cache: _, rate_limit: _ } => {
            // TODO: honor per-rule rate_limit. Today we still enforce the
            // global per-app bucket inside handle_dispatch.
            // RPC requires the X-Api-Key check. SSR is open by default
            // (the app's own pages can call it; gating is in user code).
            if mode == WorkerMode::Rpc {
                if let Err(resp) = auth::check_api_key(&req, route) {
                    return resp;
                }
            }
            handle_dispatch(req, &state, app_id, route, tail, body, wall_start).await
        }
        Outcome::Static(hit) => serve_static_hit(&state, app_id, hit, wall_start).await,
        Outcome::Redirect { to, status } => {
            let st = ntex::http::StatusCode::from_u16(status)
                .unwrap_or(ntex::http::StatusCode::FOUND);
            HttpResponse::build(st)
                .header("location", to)
                .header(
                    "x-wall-time-ms",
                    format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
                )
                .finish()
        }
        Outcome::NotFound => HttpResponse::NotFound()
            .json(&serde_json::json!({"error": "no rule matched"})),
    }
}

/// Serve a [`StaticHit`] from the BundleStore via the control plane's
/// asset endpoint.
async fn serve_static_hit(
    state: &GateState,
    app_id: &Uuid,
    hit: crate::dispatch::StaticHit,
    wall_start: std::time::Instant,
) -> HttpResponse {
    let asset_path = hit.path.trim_start_matches('/');
    match fetch_asset(&state.config.control_url, app_id, asset_path).await {
        Ok((200, data)) => {
            let status = hit.status.unwrap_or(200);
            let st = ntex::http::StatusCode::from_u16(status)
                .unwrap_or(ntex::http::StatusCode::OK);
            let mut resp = HttpResponse::build(st);
            resp.content_type(hit.content_type);
            resp.header("etag", format!("\"{}\"", hit.hash));
            resp.header("cache-control", cache_control_header(&hit.cache));
            resp.header(
                "x-wall-time-ms",
                format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
            );
            resp.body(data)
        }
        Ok((404, _)) => HttpResponse::NotFound()
            .json(&serde_json::json!({"error": "asset bytes missing"})),
        Ok((status, body)) => HttpResponse::build(
            ntex::http::StatusCode::from_u16(status)
                .unwrap_or(ntex::http::StatusCode::INTERNAL_SERVER_ERROR),
        )
        .body(body),
        Err(_) => HttpResponse::ServiceUnavailable()
            .json(&serde_json::json!({"error": "control plane unavailable"})),
    }
}

/// Build the `Cache-Control` header value from a [`CacheCtl`].
fn cache_control_header(c: &zeroship_core::types::CacheCtl) -> String {
    let mut parts: Vec<String> = vec!["public".into(), format!("max-age={}", c.max_age)];
    if let Some(swr) = c.swr_window {
        parts.push(format!("stale-while-revalidate={swr}"));
    }
    if c.immutable {
        parts.push("immutable".into());
    }
    parts.join(", ")
}

// ---------------------------------------------------------------------------
// Dispatch handler — the single worker-facing path
// ---------------------------------------------------------------------------

/// Forward an HTTP request to the worker via `/dispatch/{app_id}`.
///
/// The full HTTP request (method, URL, headers, body) is packaged into the
/// HttpEnvelope and handed to `Runtime::call_fetch_handler`, which invokes
/// the app's exported `default.fetch(req, env, ctx)`. Covers both the
/// `_rpc/*` URLs (routed inside the kernel via the bootstrap) and plain
/// HTTP requests. Enables streaming responses (e.g., SSE for LLM token
/// streaming).
async fn handle_dispatch(
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
            .map(|u| user_auth::encode_user_header(&u, &state.config.worker_key))
    } else {
        None
    };

    // Reconstruct the URL the JS handler will see.
    let scheme = if req.connection_info().scheme() == "https" { "https" } else { "http" };
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let url = format!("{scheme}://{host}/{tail}");

    // Collect request headers as [key, value] pairs.
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in req.headers() {
        if let Ok(v) = value.to_str() {
            headers.push((name.as_str().to_string(), v.to_string()));
        }
    }

    let method = req.method().as_str();
    let body_str = String::from_utf8_lossy(&body);

    // Proxy to worker via CHWBL hash ring.
    let request_id = Uuid::new_v4();
    let mut response = match proxy::forward_dispatch(
        &state.hash_ring,
        app_id,
        &route.plan_id,
        &request_id,
        method,
        &url,
        &headers,
        &body_str,
        user_header_value.as_deref(),
        &state.config.worker_key,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return HttpResponse::BadGateway()
                .json(&serde_json::json!({"error": format!("worker error: {e}")}));
        }
    };

    // Handle 401 response: redirect browser requests to the auth page.
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

    // Add response headers.
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

