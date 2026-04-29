use std::sync::Arc;

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
        Outcome::Static(hit) => serve_static_hit(&state, hit, wall_start).await,
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

/// Outcome of the cache → blob-store fetch for a static hit. Pulled out
/// so it can be unit-tested with a `MockBlobStore` without constructing a
/// full `GateState`.
enum BlobFetch {
    Hit(bytes::Bytes),
    NotFound,
    Unavailable(String),
}

/// Resolve a blob through the in-memory LRU first, falling back to the
/// underlying store and filling the cache on miss.
async fn fetch_static_bytes(
    cache: &crate::blob_cache::BlobCache,
    store: &dyn zeroship_core::blob::BlobStore,
    hash: &str,
) -> BlobFetch {
    if let Some(b) = cache.get(hash) {
        return BlobFetch::Hit(b);
    }
    match store.get_blob(hash).await {
        Ok(b) => {
            cache.insert(hash.to_string(), b.clone());
            BlobFetch::Hit(b)
        }
        Err(zeroship_core::blob::BlobError::NotFound(_)) => BlobFetch::NotFound,
        Err(e) => BlobFetch::Unavailable(e.to_string()),
    }
}

/// Serve a [`StaticHit`] from the gateway's blob cache, falling back to
/// the underlying [`BlobStore`]. No HTTP round-trip to the control
/// plane on the hot path.
async fn serve_static_hit(
    state: &GateState,
    hit: crate::dispatch::StaticHit,
    wall_start: std::time::Instant,
) -> HttpResponse {
    let bytes = match fetch_static_bytes(&state.blob_cache, &*state.blob_store, &hit.hash).await {
        BlobFetch::Hit(b) => b,
        BlobFetch::NotFound => {
            return HttpResponse::NotFound()
                .json(&serde_json::json!({"error": "asset bytes missing"}));
        }
        BlobFetch::Unavailable(err) => {
            eprintln!("[gate] blob fetch error for {}: {err}", hit.hash);
            return HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": "blob store unavailable"}));
        }
    };
    let status = hit.status.unwrap_or(200);
    let st = ntex::http::StatusCode::from_u16(status).unwrap_or(ntex::http::StatusCode::OK);
    let mut resp = HttpResponse::build(st);
    resp.content_type(hit.content_type.clone());
    resp.header("etag", format!("\"{}\"", hit.hash));
    resp.header("cache-control", cache_control_header(&hit.cache));
    resp.header(
        "x-wall-time-ms",
        format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
    );
    // ntex's body() wants ntex_bytes::Bytes; Phase B will switch to mmap
    // and emit zero-copy via Bytes::from_owner.
    resp.body(Bytes::copy_from_slice(&bytes))
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

// ---------------------------------------------------------------------------
// Tests — cache → blob_store → cache-fill path
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use crate::blob_cache::BlobCache;
    use zeroship_core::blob::{BlobError, BlobStore};

    /// In-memory `BlobStore` shim that counts `get_blob` calls so tests
    /// can assert the cache short-circuited a second fetch.
    #[derive(Debug, Default)]
    struct MockBlobStore {
        blobs: Mutex<HashMap<String, bytes::Bytes>>,
        get_calls: Mutex<HashMap<String, usize>>,
        force_unavailable: Mutex<bool>,
    }

    impl MockBlobStore {
        fn new() -> Self {
            Self::default()
        }

        fn put(&self, hash: &str, data: &[u8]) {
            self.blobs
                .lock()
                .unwrap()
                .insert(hash.to_string(), bytes::Bytes::copy_from_slice(data));
        }

        fn calls_for(&self, hash: &str) -> usize {
            *self.get_calls.lock().unwrap().get(hash).unwrap_or(&0)
        }

        fn set_unavailable(&self, on: bool) {
            *self.force_unavailable.lock().unwrap() = on;
        }
    }

    #[async_trait::async_trait(?Send)]
    impl BlobStore for MockBlobStore {
        async fn get_blob(&self, hash: &str) -> Result<bytes::Bytes, BlobError> {
            *self
                .get_calls
                .lock()
                .unwrap()
                .entry(hash.to_string())
                .or_insert(0) += 1;
            if *self.force_unavailable.lock().unwrap() {
                return Err(BlobError::Backend("synthetic outage".into()));
            }
            self.blobs
                .lock()
                .unwrap()
                .get(hash)
                .cloned()
                .ok_or_else(|| BlobError::NotFound(hash.to_string()))
        }
        fn local_path(&self, _hash: &str) -> Option<PathBuf> {
            None
        }
        async fn put_blob(&self, hash: &str, data: &[u8]) -> Result<(), BlobError> {
            self.put(hash, data);
            Ok(())
        }
        async fn has_blob(&self, hash: &str) -> Result<bool, BlobError> {
            Ok(self.blobs.lock().unwrap().contains_key(hash))
        }
        async fn put_manifest(
            &self,
            _app_id: &uuid::Uuid,
            _deploy_hash: &str,
            _json: &[u8],
        ) -> Result<(), BlobError> {
            unimplemented!("not used by the gateway")
        }
        async fn get_manifest(
            &self,
            _app_id: &uuid::Uuid,
            _deploy_hash: &str,
        ) -> Result<bytes::Bytes, BlobError> {
            unimplemented!("not used by the gateway")
        }
    }

    #[compio::test]
    async fn miss_falls_through_to_blob_store_and_fills_cache() {
        let cache = BlobCache::new(1024);
        let store = MockBlobStore::new();
        let hash = "h1";
        store.put(hash, b"hello world");

        let r = fetch_static_bytes(&cache, &store, hash).await;
        match r {
            BlobFetch::Hit(b) => assert_eq!(&b[..], b"hello world"),
            other => panic!("expected Hit, got {other:?}"),
        }
        assert_eq!(store.calls_for(hash), 1);
        // Cache was filled — second call must NOT reach the store.
        let r = fetch_static_bytes(&cache, &store, hash).await;
        assert!(matches!(r, BlobFetch::Hit(_)));
        assert_eq!(store.calls_for(hash), 1, "second hit must come from cache");
    }

    #[compio::test]
    async fn missing_blob_yields_not_found() {
        let cache = BlobCache::new(1024);
        let store = MockBlobStore::new();
        let r = fetch_static_bytes(&cache, &store, "nope").await;
        assert!(matches!(r, BlobFetch::NotFound));
        // Cache must stay empty on NotFound — otherwise a transient deploy
        // race would poison the cache.
        assert_eq!(cache.len(), 0);
    }

    #[compio::test]
    async fn store_error_yields_unavailable_and_does_not_cache() {
        let cache = BlobCache::new(1024);
        let store = MockBlobStore::new();
        store.put("h1", b"abc");
        store.set_unavailable(true);
        let r = fetch_static_bytes(&cache, &store, "h1").await;
        assert!(matches!(r, BlobFetch::Unavailable(_)));
        assert_eq!(cache.len(), 0);
    }

    impl std::fmt::Debug for BlobFetch {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Hit(b) => f.debug_tuple("Hit").field(&b.len()).finish(),
                Self::NotFound => f.write_str("NotFound"),
                Self::Unavailable(s) => f.debug_tuple("Unavailable").field(s).finish(),
            }
        }
    }
}
