use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use ntex::http::body::SizedStream;
use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use uuid::Uuid;

use crate::{auth, enforce, proxy, user_auth, GateState};

// ---------------------------------------------------------------------------
// Phase 6 — streaming static-asset serving
// ---------------------------------------------------------------------------

// Blobs at or above this size skip the in-memory cache and stream from
// disk in fixed-size chunks. Holding 5 MB-plus assets in `BlobCache`
// would either evict everything else (single-entry budget bypass) or
// be silently dropped (single-entry budget exceeded), so streaming is
// both a correctness and a footprint win for large blobs. 1 MiB
// matches the value cited in `docs/architecture/blob-store.md`
// "Phase C" and is a clean cut-off between "small enough to share
// via Bytes refcounting" and "large enough to pay the cost of
// chunked file I/O".
const STREAM_THRESHOLD_BYTES: u64 = 1024 * 1024;

// Chunk size for the streaming reader. 64 KiB is the historical
// `sendfile`-friendly default — large enough that read overhead
// amortises, small enough that one slow client can't pin tens of MB
// of RSS while a fast disk feeds it.
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

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
// Per-rule rate-limit bucket key derivation
// ---------------------------------------------------------------------------

/// Resolve the bucket discriminator for a per-rule rate limit. The
/// returned string is concatenated with `(app_id, rule_idx)` in
/// `PerRuleKey` to form the bucket key — clients sharing the same
/// discriminator share a token bucket.
///
/// * `RateLimitPer::Ip` — request's client IP. Falls back to "unknown"
///   when the connection has no peer address (test fixtures, exotic
///   transports). Reads from `connection_info().remote()` so a trusted
///   proxy's `X-Forwarded-For` is honored when present (matches what
///   the gateway already does for scheme detection a few lines below).
/// * `RateLimitPer::Session` — the `__zs_session` cookie value.
///   Anonymous callers (no cookie) fall back to the IP so an
///   unauthenticated burst still gets bucketed; without the fallback
///   they'd all share one "" key.
/// * `RateLimitPer::App` — constant `"app"`. One bucket platform-wide;
///   `(app_id, rule_idx, "app")` is the key, equivalent to a global
///   per-app limit at the rule level.
pub(crate) fn compute_bucket_id(
    req: &HttpRequest,
    per: zeroship_core::types::RateLimitPer,
) -> String {
    use zeroship_core::types::RateLimitPer;
    match per {
        RateLimitPer::Ip => req
            .connection_info()
            .remote()
            .unwrap_or("unknown")
            .to_string(),
        RateLimitPer::Session => {
            let cookie = req
                .headers()
                .get("cookie")
                .and_then(|v| v.to_str().ok());
            extract_session_cookie(cookie).unwrap_or_else(|| {
                req.connection_info()
                    .remote()
                    .unwrap_or("unknown")
                    .to_string()
            })
        }
        RateLimitPer::App => "app".to_string(),
    }
}

/// Pull the `__zs_session` value out of a Cookie header. Returns
/// `None` when the cookie is missing or empty so callers can fall
/// back to a different discriminator.
fn extract_session_cookie(cookie_header: Option<&str>) -> Option<String> {
    let s = cookie_header?;
    let token = s
        .split(';')
        .map(|p| p.trim())
        .find(|p| p.starts_with("__zs_session="))?
        .strip_prefix("__zs_session=")?;
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
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
    let dispatch_path = format!("/{tail}");

    // CORS preflight short-circuit. The browser sends `OPTIONS` with
    // `Origin` and `Access-Control-Request-Method` *before* the actual
    // request; we look up the resource-tree CORS policy for the
    // requested path and answer with a 204. If no resource matches,
    // fall through to normal dispatch (which will return 404).
    if req.method() == ntex::http::Method::OPTIONS && req.headers().contains_key("origin") {
        if let Some(policy) = compiled_route.manifest.lookup_resource(&dispatch_path) {
            if let Some(cors) = &policy.cors {
                let origin = req
                    .headers()
                    .get("origin")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                return build_preflight_response(cors, origin, wall_start);
            }
        }
    }

    // Resource-tree dispatch — resources is the only dispatch path.
    // No match → 404 (the `*` catch-all in resources should always
    // match if the user wants a fallback handler).
    if compiled_route.manifest.lookup_resource(&dispatch_path).is_some() {
        return execute_resource_tree(
            req,
            state,
            &app_id,
            &compiled_route,
            &dispatch_path,
            tail,
            body,
            wall_start,
        )
        .await;
    }

    HttpResponse::NotFound()
        .json(&serde_json::json!({"error": "no resource matched"}))
}

// ---------------------------------------------------------------------------
// v3 resource-tree dispatch
// ---------------------------------------------------------------------------

/// Run the v3 resource-tree dispatch for a request that resolved to a
/// manifest with `resources` non-empty. Looks up the matching resource,
/// enforces the precomputed `EffectivePolicy`, and executes the
/// resolved action (worker forward / redirect / static).
async fn execute_resource_tree(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    app_id: &Uuid,
    compiled_route: &crate::sync::CompiledRoute,
    dispatch_path: &str,
    tail: &str,
    body: Bytes,
    wall_start: std::time::Instant,
) -> HttpResponse {
    use crate::compiled::ResolvedAction;
    use zeroship_core::types::ProcedureKind;

    // 1. Resolve the resource. No match → 404.
    let Some(policy) = compiled_route.manifest.lookup_resource(dispatch_path) else {
        return HttpResponse::NotFound()
            .json(&serde_json::json!({"error": "no resource matched"}));
    };

    // Capture origin once for downstream CORS injection.
    let origin_value = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // 2. Method-vs-kind gate (RPC procedures only).
    //    Spec §7.1: `kind: "mutation"` cannot be served via GET.
    //    Streams / subscriptions are out of scope for Phase 2 method
    //    gating — they negotiate via SSE / WebSocket headers.
    if let Some(kind) = policy.kind {
        let method = req.method();
        let allow = match kind {
            ProcedureKind::Query => {
                method == ntex::http::Method::GET
                    || method == ntex::http::Method::POST
                    || method == ntex::http::Method::HEAD
            }
            ProcedureKind::Mutation => method == ntex::http::Method::POST,
            ProcedureKind::Stream | ProcedureKind::Subscription => true,
        };
        if !allow {
            return HttpResponse::MethodNotAllowed()
                .json(&serde_json::json!({"error": "method not allowed for this procedure kind"}));
        }
    }

    // 3. Auth gate. `anon` always passes (subject to publicly_accessible
    //    being set, which is enforced at validate-time). `user`/`admin`
    //    require a session cookie — Phase 2 reuses the existing
    //    `__zs_session` extraction; richer admin-vs-user role checks
    //    will arrive when the auth tier ships.
    if !auth_satisfied(&req, policy, &state.config.auth_secret, app_id) {
        return HttpResponse::Unauthorized()
            .json(&serde_json::json!({"error": "authentication required"}));
    }

    // 4. CSRF origin guard. Mutations with a declared csrf_origins list
    //    require the request's `Origin` to match.
    if matches!(policy.kind, Some(ProcedureKind::Mutation))
        || req.method() == ntex::http::Method::POST
        || req.method() == ntex::http::Method::PUT
        || req.method() == ntex::http::Method::PATCH
        || req.method() == ntex::http::Method::DELETE
    {
        if let Some(allowed) = &policy.csrf_origins {
            let origin = origin_value.as_deref().unwrap_or("");
            if !allowed.iter().any(|o| o == origin) {
                return HttpResponse::Forbidden()
                    .json(&serde_json::json!({"error": "origin not in csrf_origins allow list"}));
            }
        }
    }

    // 5. Max-input-bytes guard. Cheap when not set; cap the body size
    //    before forwarding to the worker.
    if let Some(cap) = policy.max_input_bytes {
        if body.len() > cap as usize {
            return HttpResponse::PayloadTooLarge()
                .json(&serde_json::json!({"error": "input exceeds max_input_bytes"}));
        }
    }

    // 6. Per-resource rate-limit (when declared).
    //    Reuses the existing `PerRuleRateLimitRegistry` by hashing the
    //    resource key into a stable `rule_idx`. Two distinct resource
    //    keys with the same rate_limit shape get independent buckets.
    if let Some(rl) = &policy.rate_limit {
        let resource_key = compiled_route
            .manifest
            .lookup_resource_key(dispatch_path)
            .unwrap_or_default();
        let rule_idx = resource_key_hash(&resource_key);
        let bucket_id = compute_bucket_id(&req, rl.per);
        if let Err(resp) = state.per_rule_rate_limits.check(
            app_id,
            rule_idx,
            rl.per,
            &bucket_id,
            rl,
        ) {
            return resp;
        }
    }

    // 7. Execute the resolved action.
    let mut response = match &policy.action {
        ResolvedAction::WorkerRpc | ResolvedAction::WorkerSsr => {
            handle_dispatch(
                req,
                &state,
                app_id,
                &compiled_route.entry,
                tail,
                body,
                wall_start,
            )
            .await
        }
        ResolvedAction::Redirect { to, status } => {
            let st = ntex::http::StatusCode::from_u16(*status)
                .unwrap_or(ntex::http::StatusCode::FOUND);
            HttpResponse::build(st)
                .header("location", to.clone())
                .header(
                    "x-wall-time-ms",
                    format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
                )
                .finish()
        }
        ResolvedAction::Rewrite { to } => {
            // Phase 2: rewrite forwards under the new path; recursion
            // not yet supported (would need to re-enter lookup_resource
            // with hop-limiting). Pass-through to worker for now.
            let _ = to;
            handle_dispatch(
                req,
                &state,
                app_id,
                &compiled_route.entry,
                tail,
                body,
                wall_start,
            )
            .await
        }
        ResolvedAction::Static { try_chain } => {
            // For Phase 2 we resolve the first asset that exists in
            // `assets` / `runtime_assets`. The legacy walker has more
            // sophisticated `$path` / `[capture]` substitution; for the
            // resource-tree path the build emits literal templates.
            serve_resource_tree_static(
                &state,
                compiled_route,
                &req,
                dispatch_path,
                try_chain,
                wall_start,
            )
            .await
        }
    };

    // 8. CORS injection on the response (resource-tree's flattened
    //    `cors`).
    if let (Some(cors), Some(origin)) = (policy.cors.as_ref(), origin_value.as_deref()) {
        if !origin.is_empty() {
            inject_cors_response_headers(response.headers_mut(), cors, origin);
        }
    }

    response
}

/// Decide whether `req` satisfies `policy.auth`. `Anon` always passes
/// (validate() enforces `publicly_accessible: true`). `User` and `Admin`
/// require a verifiable `__zs_session` cookie. Phase 2 doesn't yet
/// distinguish admin from user roles — that ships with the auth tier
/// rework. For now both require a session.
fn auth_satisfied(
    req: &HttpRequest,
    policy: &crate::compiled::EffectivePolicy,
    auth_secret: &str,
    app_id: &Uuid,
) -> bool {
    use zeroship_core::types::AuthLevel;
    if matches!(policy.auth, AuthLevel::Anon) {
        return true;
    }
    if auth_secret.is_empty() {
        // Dev / test: when there's no auth secret configured the gateway
        // can't verify a cookie. Allow the request through; the worker
        // can still apply finer-grained checks. Matches the behavior of
        // the legacy path where `auth_secret.is_empty()` skips user
        // header injection.
        return true;
    }
    let cookie = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok());
    let app_id_str = app_id.to_string();
    user_auth::extract_user(cookie, auth_secret, &app_id_str).is_some()
}

/// Hash a resource key into a stable u32 for use as the
/// `PerRuleRateLimitRegistry` rule_idx. xxh3 keeps the
/// hash deterministic across processes; we truncate to u32 because the
/// registry's bucket key only differentiates by `(app_id, rule_idx,
/// bucket)` and a 32-bit space is plenty for the per-app resource set.
fn resource_key_hash(key: &str) -> u32 {
    xxhash_rust::xxh3::xxh3_64(key.as_bytes()) as u32
}

/// Resolve a static action's `try` chain against the manifest's asset
/// maps. First entry that hits wins; returns 404 on full miss.
async fn serve_resource_tree_static(
    state: &GateState,
    compiled_route: &crate::sync::CompiledRoute,
    req: &HttpRequest,
    request_path: &str,
    try_chain: &[String],
    wall_start: std::time::Instant,
) -> HttpResponse {
    // Phase 2: support the literal templates the build emits, plus the
    // bare `$path` token (used by `/_assets/*` SPA fallback). Captures
    // are deferred — the build emits literal paths for now.
    for tpl in try_chain {
        let resolved = if tpl == "$path" {
            request_path.to_string()
        } else {
            tpl.clone()
        };
        if let Some(hit) = lookup_static_hit(compiled_route, &resolved) {
            return serve_static_hit(state, req, hit, wall_start).await;
        }
    }
    HttpResponse::NotFound().json(&serde_json::json!({"error": "asset not found"}))
}

/// Pull a [`StaticHit`] from either runtime_assets or assets via the
/// compiled manifest, building cache directives the same way the legacy
/// walker does.
fn lookup_static_hit(
    compiled_route: &crate::sync::CompiledRoute,
    path: &str,
) -> Option<crate::dispatch::StaticHit> {
    use zeroship_core::types::CacheCtl;
    let (entry, mutable) = compiled_route.manifest.lookup_asset_for_static(path)?;
    let cache = entry.cache.clone().unwrap_or_else(|| {
        if !mutable && path.starts_with("/_assets/") {
            CacheCtl {
                max_age: 31_536_000,
                swr_window: None,
                immutable: true,
                background_refresh: false,
                stale_on_error: false,
            }
        } else {
            CacheCtl {
                max_age: 60,
                swr_window: None,
                immutable: false,
                background_refresh: false,
                stale_on_error: false,
            }
        }
    });
    Some(crate::dispatch::StaticHit {
        path: path.to_string(),
        hash: entry.hash.clone(),
        content_type: entry.content_type.clone(),
        size: entry.size,
        cache,
        status: None,
        mutable,
        variants: entry.variants.clone(),
    })
}

// ---------------------------------------------------------------------------
// Manifest-driven outcome execution
// ---------------------------------------------------------------------------

/// Build a 204 preflight response for a matching CORS rule. Called when
/// the request is `OPTIONS` with an `Origin` header AND a CORS-bearing
/// rule matches the `Access-Control-Request-Method` + path. The body is
/// empty; the headers tell the browser whether to proceed with the
/// actual request.
fn build_preflight_response(
    cors: &zeroship_core::types::Cors,
    origin: &str,
    wall_start: std::time::Instant,
) -> HttpResponse {
    use ntex::http::StatusCode;
    let mut resp = HttpResponse::build(StatusCode::NO_CONTENT);
    // Allow-Origin: "*" only when credentials disabled; specific origin
    // when in the explicit allow list. Anything else → no Allow-Origin
    // header at all and the browser blocks.
    let wildcard_ok = cors.allow_origins.iter().any(|o| o == "*") && !cors.allow_credentials;
    let exact_match = cors.allow_origins.iter().any(|o| o == origin);
    if wildcard_ok {
        resp.header("access-control-allow-origin", "*");
    } else if exact_match {
        resp.header("access-control-allow-origin", origin);
        resp.header("vary", "Origin");
    }
    if (wildcard_ok || exact_match) && !cors.allow_methods.is_empty() {
        let methods = cors
            .allow_methods
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        resp.header("access-control-allow-methods", methods);
    }
    if (wildcard_ok || exact_match) && !cors.allow_headers.is_empty() {
        resp.header(
            "access-control-allow-headers",
            cors.allow_headers.join(", "),
        );
    }
    if cors.allow_credentials && exact_match {
        resp.header("access-control-allow-credentials", "true");
    }
    if let Some(seconds) = cors.max_age_seconds {
        if wildcard_ok || exact_match {
            resp.header("access-control-max-age", seconds.to_string());
        }
    }
    resp.header(
        "x-wall-time-ms",
        format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
    );
    resp.finish()
}

/// Inject `Access-Control-*` response headers on a non-preflight
/// response. Mirrors the preflight logic for Allow-Origin/Vary; also
/// emits `Expose-Headers` and `Allow-Credentials` so the browser exposes
/// the right response surface.
fn inject_cors_response_headers(
    headers: &mut ntex::http::HeaderMap,
    cors: &zeroship_core::types::Cors,
    origin: &str,
) {
    use ntex::http::header::{HeaderName, HeaderValue};
    let wildcard_ok = cors.allow_origins.iter().any(|o| o == "*") && !cors.allow_credentials;
    let exact_match = cors.allow_origins.iter().any(|o| o == origin);
    if wildcard_ok {
        if let Ok(v) = HeaderValue::from_str("*") {
            headers.insert(HeaderName::from_static("access-control-allow-origin"), v);
        }
    } else if exact_match {
        if let Ok(v) = HeaderValue::from_str(origin) {
            headers.insert(HeaderName::from_static("access-control-allow-origin"), v);
        }
        headers.insert(
            HeaderName::from_static("vary"),
            HeaderValue::from_static("Origin"),
        );
    } else {
        // Origin not in allow list → no headers; the browser blocks.
        return;
    }
    if !cors.expose_headers.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&cors.expose_headers.join(", ")) {
            headers.insert(
                HeaderName::from_static("access-control-expose-headers"),
                v,
            );
        }
    }
    if cors.allow_credentials && exact_match {
        headers.insert(
            HeaderName::from_static("access-control-allow-credentials"),
            HeaderValue::from_static("true"),
        );
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

/// Resolve a blob through the gateway's three-tier cache:
///
/// 1. Memory LRU — `BlobCache::get` returns refcounted `Bytes`.
/// 2. Disk LRU — `DiskBlobCache::local_path` returns a path; we
///    `mmap` it and wrap as `Bytes::from_owner(mmap)` for zero-copy
///    serving (Phase B).
/// 3. Backend — fetch from `BlobStore`, fill both tiers.
///
/// On a disk miss + backend hit we ALWAYS write to the disk cache so
/// subsequent serves take the mmap path. The mem cache is also filled
/// so hot blobs short-circuit before disk I/O.
async fn fetch_static_bytes(
    mem: &crate::blob_cache::BlobCache,
    disk: &crate::blob_cache::DiskBlobCache,
    store: &dyn zeroship_core::blob::BlobStore,
    hash: &str,
) -> BlobFetch {
    // Tier 1: memory.
    if let Some(b) = mem.get(hash) {
        return BlobFetch::Hit(b);
    }
    // Tier 2: disk (mmap).
    if let Some(path) = disk.local_path(hash) {
        match crate::blob_cache::mmap_to_bytes(&path) {
            Ok(b) => {
                mem.insert(hash.to_string(), b.clone());
                return BlobFetch::Hit(b);
            }
            Err(e) => {
                // The on-disk file may have been unlinked under us
                // (eviction race) or the FS could be sick. Don't
                // panic — fall through to the backend and let it
                // refill both tiers.
                eprintln!("[gate] mmap failed for {hash}: {e}");
            }
        }
    }
    // Tier 3: backend.
    match store.get_blob(hash).await {
        Ok(b) => {
            // Best-effort disk fill: a failure here doesn't stop the
            // serve. The mem tier still gets the bytes.
            if let Err(e) = disk.insert(hash, &b) {
                eprintln!("[gate] disk cache insert failed for {hash}: {e}");
            }
            mem.insert(hash.to_string(), b.clone());
            BlobFetch::Hit(b)
        }
        Err(zeroship_core::blob::BlobError::NotFound(_)) => BlobFetch::NotFound,
        Err(e) => BlobFetch::Unavailable(e.to_string()),
    }
}

/// Outcome of `ensure_disk_path` — the streaming path needs the file
/// available locally, but on a backend-fetch fallback we may already
/// have the bytes in hand and a disk insert may have failed. Callers
/// fall back to a buffered response in that case.
enum DiskAvailability {
    /// File is on disk at this path; safe to mmap or stream.
    OnDisk(PathBuf),
    /// File is not on disk (insert failed) but we have the bytes —
    /// caller must serve them buffered.
    InMemoryOnly(bytes::Bytes),
    NotFound,
    Unavailable(String),
}

/// Make sure the blob is available on the disk LRU and return its
/// path. On a disk miss we fetch from the backend and write to disk;
/// if the disk insert fails (e.g. ENOSPC) we still hand back the
/// bytes so the caller can serve a buffered response. The mem cache
/// is intentionally NOT touched here — large blobs that take this
/// path would otherwise either bypass the per-entry budget cap or
/// silently fail to cache, neither of which is useful.
async fn ensure_disk_path(
    disk: &crate::blob_cache::DiskBlobCache,
    store: &dyn zeroship_core::blob::BlobStore,
    hash: &str,
) -> DiskAvailability {
    if let Some(path) = disk.local_path(hash) {
        return DiskAvailability::OnDisk(path);
    }
    match store.get_blob(hash).await {
        Ok(b) => match disk.insert(hash, &b) {
            Ok(()) => match disk.local_path(hash) {
                Some(p) => DiskAvailability::OnDisk(p),
                // Insert succeeded but the LRU dropped it on its way
                // back out (e.g. another concurrent insert pushed it
                // past the budget). Fall back to in-memory.
                None => DiskAvailability::InMemoryOnly(b),
            },
            Err(e) => {
                eprintln!("[gate] disk cache insert failed for {hash}: {e}");
                DiskAvailability::InMemoryOnly(b)
            }
        },
        Err(zeroship_core::blob::BlobError::NotFound(_)) => DiskAvailability::NotFound,
        Err(e) => DiskAvailability::Unavailable(e.to_string()),
    }
}

/// Spawn a compio task that reads `path` in `chunk_bytes`-sized chunks
/// starting at offset 0 and pushes each chunk into the returned mpsc
/// receiver. The receiver yields `Result<Bytes, Rc<dyn Error>>` so it
/// can be plumbed straight into ntex's `SizedStream` body type.
///
/// The task drops the file (and stops reading) the moment the receiver
/// goes away — a slow-client disconnect won't keep reading bytes from
/// disk indefinitely. Reads use `compio::fs::File::read_at`, which on
/// Linux maps to `IORING_OP_READ` (real positional async I/O, no
/// blocking thread pool).
///
/// Splitting the read loop into a separate function makes the test
/// surface narrow: `chunk_stream_from_file` is generic over the source
/// type, so we can substitute an in-memory `Vec<u8>` (which already
/// implements `compio_io::AsyncReadAt`) and verify chunk sizing without
/// touching the filesystem.
fn chunk_stream_from_path(
    path: PathBuf,
    size: u64,
    chunk_bytes: usize,
) -> ntex::channel::mpsc::Receiver<Result<Bytes, Rc<dyn std::error::Error>>> {
    chunk_stream_from_path_range(path, 0, size, chunk_bytes)
}

/// Range-aware variant of [`chunk_stream_from_path`]. Reads `length`
/// bytes starting at `start` from the file, in `chunk_bytes`-sized
/// chunks. Used for `Range:` requests on the streaming path so we
/// only ship the requested slice.
fn chunk_stream_from_path_range(
    path: PathBuf,
    start: u64,
    length: u64,
    chunk_bytes: usize,
) -> ntex::channel::mpsc::Receiver<Result<Bytes, Rc<dyn std::error::Error>>> {
    let (tx, rx) = ntex::channel::mpsc::channel();
    compio::runtime::spawn(async move {
        let file = match compio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(Err::<Bytes, Rc<dyn std::error::Error>>(Rc::new(e)));
                return;
            }
        };
        chunk_stream_from_file_range(&file, start, length, chunk_bytes, &tx).await;
    })
    .detach();
    rx
}

/// Drain a chunk-by-chunk read of `source` into `tx`. Pulled out of
/// `chunk_stream_from_path` so tests can drive it with an in-memory
/// `Vec<u8>` (which implements `compio_io::AsyncReadAt`) without
/// spinning up a real file. The function exits early when:
///
/// * `tx.send` returns `Err` — the client disconnected, no point
///   reading more.
/// * The source returns 0 bytes — short read; assume EOF.
/// * The source returns an error — propagate it as the final stream
///   item, then close.
#[cfg(test)]
async fn chunk_stream_from_file<R>(
    source: &R,
    size: u64,
    chunk_bytes: usize,
    tx: &ntex::channel::mpsc::Sender<Result<Bytes, Rc<dyn std::error::Error>>>,
) where
    R: compio::io::AsyncReadAt,
{
    chunk_stream_from_file_range(source, 0, size, chunk_bytes, tx).await
}

/// Range-aware variant of [`chunk_stream_from_file`]. Starts reading
/// at `start` and emits exactly `length` bytes (or fewer on a short
/// read / error). Same exit conditions as the non-range version.
async fn chunk_stream_from_file_range<R>(
    source: &R,
    start: u64,
    length: u64,
    chunk_bytes: usize,
    tx: &ntex::channel::mpsc::Sender<Result<Bytes, Rc<dyn std::error::Error>>>,
) where
    R: compio::io::AsyncReadAt,
{
    use compio::buf::BufResult;
    let mut sent: u64 = 0;
    while sent < length {
        let want = std::cmp::min(chunk_bytes as u64, length - sent) as usize;
        let buf = vec![0u8; want];
        let BufResult(res, returned) = source.read_at(buf, start + sent).await;
        match res {
            Ok(0) => return,
            Ok(n) => {
                // Trim the buffer to what was actually read — short
                // reads are legal and we don't want to ship trailing
                // zeros to the client.
                let mut chunk = returned;
                chunk.truncate(n);
                // Bytes::from(Vec<u8>) copies in this version of
                // ntex_bytes. That's the residual user-space copy
                // Phase C / sendfile would eliminate. The win at this
                // layer is that we never hold the full file in RAM.
                if tx.send(Ok(Bytes::from(chunk))).is_err() {
                    return;
                }
                sent += n as u64;
            }
            Err(e) => {
                let _ = tx.send(Err::<Bytes, Rc<dyn std::error::Error>>(Rc::new(e)));
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP completeness helpers (Tier 4a)
//   * `If-None-Match` → 304 short-circuit
//   * `Range:` parsing (single, suffix, open-end, multi, unsatisfiable)
//   * `Cache-Control` extensions (`stale-if-error`)
//   * `Accept-Ranges: bytes` advertised on every static response
//
// Plus Tier 4b — pre-compressed variants:
//   * `pick_variant` for `Accept-Encoding` negotiation (br > gzip > identity)
//   * ETag, Content-Length, Content-Range are all per-VARIANT
//   * `Content-Encoding` + `Vary: Accept-Encoding` set when a variant fires
// ---------------------------------------------------------------------------

/// Result of `Accept-Encoding` negotiation against an asset's
/// `variants` map. Three shapes:
///
/// * `(hash, size, None)`         — identity body. The caller emits no
///                                  `Content-Encoding` and no `Vary`.
/// * `(hash, size, Some(enc))`    — variant body. Caller sets
///                                  `Content-Encoding: <enc>` and
///                                  `Vary: Accept-Encoding`.
#[derive(Debug, Clone)]
struct ChosenVariant {
    hash: String,
    size: u64,
    /// `None` → identity. `Some(enc)` → compressed variant.
    encoding: Option<String>,
}

/// Pick the best encoding for the request's `Accept-Encoding` against
/// the asset's available variants. Falls back to identity when no
/// variant is offered or none of the offered encodings are accepted.
///
/// q-value handling: any encoding listed with `q=0` is treated as
/// rejected (matches RFC 7231 §5.3.4); otherwise we walk the request's
/// listed encodings in order and return the first one we have a
/// variant for. Listed-encodings order is the client's preference
/// signal — Chrome / Firefox put `br` before `gzip`, which is what
/// we want, so a simple in-order walk does the right thing without a
/// full q-value sort.
///
/// Special tokens:
/// * `*` (any) — matches any variant we have. Picked only when no
///   explicit variant was listed first.
/// * `identity` — explicitly request identity; we honour it.
fn pick_variant(hit: &crate::dispatch::StaticHit, accept_encoding: Option<&str>) -> ChosenVariant {
    let identity = ChosenVariant {
        hash: hit.hash.clone(),
        size: hit.size,
        encoding: None,
    };

    let header = match accept_encoding {
        Some(h) => h,
        None => return identity,
    };
    if hit.variants.is_empty() {
        return identity;
    }

    // Parse `Accept-Encoding` into (token, accepted?) pairs, preserving
    // request order (which captures client preference for the common
    // browser case).
    //
    // Examples:
    //   "br, gzip"            → [("br", true), ("gzip", true)]
    //   "gzip;q=0.5, br;q=1"  → [("gzip", true), ("br", true)]  (q values >0 → accept)
    //   "identity;q=0, *"     → [("identity", false), ("*", true)]
    let mut accepted: Vec<&str> = Vec::with_capacity(4);
    let mut wildcard = false;
    let mut wildcard_rejected = false;
    let mut identity_rejected = false;
    for raw in header.split(',') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        let (token, q_zero) = parse_accept_encoding_part(part);
        if token == "*" {
            if q_zero {
                wildcard_rejected = true;
            } else {
                wildcard = true;
            }
            continue;
        }
        if token.eq_ignore_ascii_case("identity") {
            if q_zero {
                identity_rejected = true;
            }
            // Identity isn't a variant — we don't push it onto the
            // accepted list; we just track its rejection.
            continue;
        }
        if !q_zero {
            accepted.push(token);
        }
    }

    // Walk the client's preferred order. First variant we have wins.
    for token in &accepted {
        if let Some(variant) = hit.variants.get(*token) {
            return ChosenVariant {
                hash: variant.hash.clone(),
                size: variant.size,
                encoding: Some((*token).to_string()),
            };
        }
    }

    // Wildcard: pick any variant we have. Prefer `br` then `gzip` for
    // determinism (browsers don't typically send wildcard, but proxies
    // and CLIs do).
    if wildcard && !wildcard_rejected {
        for enc in &["br", "gzip"] {
            if let Some(variant) = hit.variants.get(*enc) {
                return ChosenVariant {
                    hash: variant.hash.clone(),
                    size: variant.size,
                    encoding: Some((*enc).to_string()),
                };
            }
        }
    }

    // Identity rejected explicitly AND no variant matched? RFC 7231
    // says we MAY return 406 here; in practice 99% of Accept-Encoding
    // headers list `identity;q=0` only as a hint, not a hard demand,
    // and serving identity is universally accepted by the actual
    // client even when the header would technically forbid it. Match
    // browsers' permissive behaviour.
    let _ = identity_rejected;
    identity
}

/// Parse one `Accept-Encoding` token segment and return its name and
/// whether it carries `q=0`. Anything else (`q=0.5`, no q at all, …)
/// → accepted.
fn parse_accept_encoding_part(part: &str) -> (&str, bool) {
    if let Some((name, params)) = part.split_once(';') {
        let name = name.trim();
        for p in params.split(';') {
            let p = p.trim();
            if let Some(qval) = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")) {
                if let Ok(q) = qval.parse::<f32>() {
                    if q <= 0.0 {
                        return (name, true);
                    }
                }
            }
        }
        (name, false)
    } else {
        (part.trim(), false)
    }
}

/// Match an `If-None-Match` request-header value against the asset's
/// strong ETag. Accepts:
///
/// * an exact match: `If-None-Match: "<etag>"`,
/// * the wildcard `*`,
/// * a comma-separated list (`"a", "b", "c"`) — match if any element
///   matches.
///
/// **Strong-only**: `W/"…"` weak prefixes are NOT considered a match.
/// Our hashes are content-addressed (SHA-256), so every ETag we emit
/// is strong — a weak match would be lying about byte-for-byte
/// equivalence.
fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    let trimmed = if_none_match.trim();
    if trimmed == "*" {
        return true;
    }
    for part in trimmed.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        // Reject weak ETags (`W/"…"`) — strong comparison only.
        if p.starts_with("W/") || p.starts_with("w/") {
            continue;
        }
        if p == etag {
            return true;
        }
    }
    false
}

/// Parsed `Range:` request — what the client asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeSpec {
    /// `bytes=N-M` (inclusive on both ends), normalised against `size`.
    /// `start <= end < size`.
    Single(u64, u64),
    /// Multiple ranges (`bytes=0-10,20-30`). RFC 7233 allows a
    /// `multipart/byteranges` response; we degrade gracefully to a
    /// 200 + full body — still RFC-compliant.
    MultiRange,
    /// Range is past EOF (`start >= size`) or otherwise unsatisfiable.
    /// Caller emits 416 with `Content-Range: bytes */<size>`.
    Unsatisfiable,
}

/// Parse a `Range` header against the asset's identity size. Returns
/// `None` when the header is absent or syntactically broken (caller
/// falls through to a normal 200). RFC 7233 §3.1 grammar — minimal
/// subset:
///
/// * `bytes=N-M`   — inclusive range; clamped to `size - 1` on overflow.
/// * `bytes=N-`    — open-end; ends at `size - 1`.
/// * `bytes=-N`    — last `N` bytes; clamped to `size`.
/// * `bytes=A-B,C-D[, …]` — multi-range; returns `MultiRange`.
///
/// Unrecognised units (`items=…`) → `None`. Non-bytes-prefixed → `None`.
fn parse_range(
    header: Option<&ntex::http::header::HeaderValue>,
    size: u64,
) -> Option<RangeSpec> {
    let raw = header?.to_str().ok()?;
    let spec = raw.strip_prefix("bytes=")?;
    let parts: Vec<&str> = spec.split(',').map(|s| s.trim()).collect();
    if parts.is_empty() {
        return None;
    }
    if parts.len() > 1 {
        // Multi-range: caller falls through to a 200 + full body. Per
        // RFC 7233 §4.1 a server MAY ignore Range — graceful degrade.
        return Some(RangeSpec::MultiRange);
    }
    let single = parts[0];
    if single.is_empty() {
        return None;
    }

    // `-N` → last N bytes. Suffix form.
    if let Some(n_str) = single.strip_prefix('-') {
        let n: u64 = n_str.parse().ok()?;
        if n == 0 || size == 0 {
            return Some(RangeSpec::Unsatisfiable);
        }
        let n = std::cmp::min(n, size);
        return Some(RangeSpec::Single(size - n, size - 1));
    }

    let (start_str, end_str) = single.split_once('-')?;
    let start: u64 = start_str.parse().ok()?;
    if start >= size {
        return Some(RangeSpec::Unsatisfiable);
    }
    let end: u64 = if end_str.is_empty() {
        size - 1
    } else {
        let parsed: u64 = end_str.parse().ok()?;
        std::cmp::min(parsed, size - 1)
    };
    if end < start {
        return Some(RangeSpec::Unsatisfiable);
    }
    Some(RangeSpec::Single(start, end))
}

/// Pull the `If-None-Match` header off a request as a borrowed `&str`.
/// `None` when absent or non-UTF-8.
fn header_str_borrowed<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
    req.headers().get(name).and_then(|v| v.to_str().ok())
}

/// Build a 416 Range Not Satisfiable response. Includes
/// `Content-Range: bytes */<size>` per RFC 7233 §4.4.
fn build_range_not_satisfiable(size: u64, etag: &str) -> HttpResponse {
    let mut resp = HttpResponse::RangeNotSatisfiable();
    resp.header("content-range", format!("bytes */{size}"));
    resp.header("etag", etag);
    resp.header("accept-ranges", "bytes");
    resp.finish()
}

/// Build a streaming `HttpResponse` for a single static hit whose
/// bytes live on the gateway's disk LRU. Falls back to a buffered
/// response when the file isn't (or can't be) on disk.
///
/// Honours `Range:` against the streaming path — `compio::fs::File::read_at`
/// already supports a starting offset, so we only ship the requested
/// slice. Multi-range requests degrade to a 200 + full body.
async fn serve_static_streaming(
    state: &GateState,
    req: &HttpRequest,
    hit: &crate::dispatch::StaticHit,
    chosen: &ChosenVariant,
    etag: &str,
    wall_start: std::time::Instant,
) -> HttpResponse {
    let path = match ensure_disk_path(&state.disk_cache, &*state.blob_store, &chosen.hash).await {
        DiskAvailability::OnDisk(p) => p,
        DiskAvailability::InMemoryOnly(b) => {
            // Disk fill failed — fall back to buffered. Skip the
            // mem cache: a multi-MB blob would either evict
            // everything else or silently fail the budget check.
            return build_buffered_response(req, hit, chosen, etag, &b, wall_start);
        }
        DiskAvailability::NotFound => {
            return HttpResponse::NotFound()
                .json(&serde_json::json!({"error": "asset bytes missing"}));
        }
        DiskAvailability::Unavailable(err) => {
            eprintln!("[gate] blob fetch error for {}: {err}", chosen.hash);
            return HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": "blob store unavailable"}));
        }
    };

    // Range parsing — done after we know the file is available so a
    // bad range on a missing blob still yields 404 first. Range is
    // computed against the VARIANT'S size — clients see the bytes
    // we'll actually serve, not the identity bytes they'd get without
    // Accept-Encoding.
    let range = parse_range(req.headers().get("range"), chosen.size);
    match range {
        Some(RangeSpec::Unsatisfiable) => return build_range_not_satisfiable(chosen.size, etag),
        Some(RangeSpec::Single(start, end)) => {
            let length = end - start + 1;
            let rx = chunk_stream_from_path_range(path, start, length, STREAM_CHUNK_BYTES);
            let mut resp = HttpResponse::PartialContent();
            resp.content_type(hit.content_type.clone());
            resp.header("etag", etag);
            resp.header("cache-control", cache_control_header(&hit.cache));
            resp.header(
                "content-range",
                format!("bytes {start}-{end}/{}", chosen.size),
            );
            resp.header("accept-ranges", "bytes");
            apply_variant_headers(&mut resp, chosen);
            resp.header(
                "x-wall-time-ms",
                format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
            );
            return resp.body(SizedStream::new(length, rx));
        }
        // None or MultiRange → fall through to the full body.
        _ => {}
    }

    let rx = chunk_stream_from_path(path, chosen.size, STREAM_CHUNK_BYTES);
    let status = hit.status.unwrap_or(200);
    let st = ntex::http::StatusCode::from_u16(status).unwrap_or(ntex::http::StatusCode::OK);
    let mut resp = HttpResponse::build(st);
    resp.content_type(hit.content_type.clone());
    resp.header("etag", etag);
    resp.header("cache-control", cache_control_header(&hit.cache));
    resp.header("accept-ranges", "bytes");
    apply_variant_headers(&mut resp, chosen);
    resp.header(
        "x-wall-time-ms",
        format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
    );
    // SizedStream sets Content-Length and uses identity transfer
    // encoding — better for browsers and intermediaries than the
    // chunked encoding `streaming()` would produce.
    resp.body(SizedStream::new(chosen.size, rx))
}

/// Apply `Content-Encoding: <enc>` and `Vary: Accept-Encoding` to a
/// response when a non-identity variant was picked. Browsers and
/// proxies need the `Vary` so they don't cross-cache compressed and
/// identity responses for clients with different `Accept-Encoding`.
fn apply_variant_headers(resp: &mut ntex::web::HttpResponseBuilder, chosen: &ChosenVariant) {
    if let Some(enc) = &chosen.encoding {
        resp.header("content-encoding", enc.as_str());
        resp.header("vary", "Accept-Encoding");
    }
}

/// Serve a [`StaticHit`] from the gateway's blob cache, falling back to
/// the underlying [`BlobStore`]. No HTTP round-trip to the control
/// plane on the hot path.
///
/// Dispatches on `hit.size`:
///
/// * Below `STREAM_THRESHOLD_BYTES`: the legacy buffered path —
///   mem LRU → disk LRU (mmap) → backend, then write the full body
///   in one go. `Bytes` is `Arc`-refcounted so concurrent requests
///   for the same hash share the buffer.
/// * At or above the threshold: the streaming path. Reads the file
///   off the disk LRU in 64 KiB chunks via `compio::fs::File::read_at`
///   and feeds them into ntex's `SizedStream`. Skips the in-memory
///   `BlobCache` insert — large blobs would either bypass the
///   per-entry budget cap or silently fail to cache, so we don't
///   bother. The kernel page cache is the warm path here, just as
///   it is for the mmap-buffered path.
///
/// Tier 4a additions (HTTP completeness):
///
/// * `If-None-Match` → 304 short-circuit BEFORE any blob fetch. Saves
///   the byte transfer entirely on warm-cache clients.
/// * `Range:` request handling on both buffered and streaming paths.
/// * `Accept-Ranges: bytes` advertised on every 200/206/304 response.
///
/// TODO: `CacheCtl::background_refresh` is currently advisory only —
/// the gateway's blob_cache LRU doesn't distinguish "stale, refresh
/// in background" from "fresh", so we can't honour it without a
/// background-revalidation tier on top of the cache. The flag IS
/// preserved in the manifest for the day we add it.
async fn serve_static_hit(
    state: &GateState,
    req: &HttpRequest,
    hit: crate::dispatch::StaticHit,
    wall_start: std::time::Instant,
) -> HttpResponse {
    // 1. Pick the encoding variant first — ETag, Content-Length,
    //    Content-Range all reflect the variant we're going to serve.
    //    `If-None-Match` matches the per-variant ETag so a client
    //    that's already seen the brotli body can short-circuit even
    //    when the identity hash differs.
    let chosen = pick_variant(&hit, header_str_borrowed(req, "accept-encoding"));
    let etag = format!("\"{}\"", chosen.hash);

    // 2. Conditional GET — short-circuit BEFORE any blob fetch.
    //    The whole point of If-None-Match is to avoid the byte transfer.
    if let Some(if_none_match) = header_str_borrowed(req, "if-none-match") {
        if etag_matches(if_none_match, &etag) {
            return build_not_modified_with_variant(
                &etag,
                &cache_control_header(&hit.cache),
                &chosen,
                wall_start,
            );
        }
    }

    // 3. Streaming path for large blobs. Threshold check is on the
    //    VARIANT'S size — a brotli'd 5 MiB JS bundle that compresses
    //    to 800 KiB takes the buffered path, which is correct: the
    //    whole point of variant compression is making the body small
    //    enough to fit in memory cheaply.
    if chosen.size >= STREAM_THRESHOLD_BYTES {
        return serve_static_streaming(state, req, &hit, &chosen, &etag, wall_start).await;
    }

    // 4. Buffered path — fetch bytes through the cache tiers.
    let bytes = match fetch_static_bytes(
        &state.blob_cache,
        &state.disk_cache,
        &*state.blob_store,
        &chosen.hash,
    )
    .await
    {
        BlobFetch::Hit(b) => b,
        BlobFetch::NotFound => {
            return HttpResponse::NotFound()
                .json(&serde_json::json!({"error": "asset bytes missing"}));
        }
        BlobFetch::Unavailable(err) => {
            eprintln!("[gate] blob fetch error for {}: {err}", chosen.hash);
            return HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": "blob store unavailable"}));
        }
    };
    build_buffered_response(req, &hit, &chosen, &etag, &bytes, wall_start)
}

/// 304-with-variant — same headers as `build_not_modified` plus
/// `Content-Encoding` / `Vary: Accept-Encoding` when a variant was
/// the negotiated body. Required by RFC 7232 §4.1: 304 must include
/// any header the corresponding 200 would have, including Vary.
fn build_not_modified_with_variant(
    etag: &str,
    cache_ctl: &str,
    chosen: &ChosenVariant,
    wall_start: std::time::Instant,
) -> HttpResponse {
    let mut resp = HttpResponse::NotModified();
    resp.header("etag", etag);
    resp.header("cache-control", cache_ctl);
    resp.header("accept-ranges", "bytes");
    apply_variant_headers(&mut resp, chosen);
    resp.header(
        "x-wall-time-ms",
        format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
    );
    resp.finish()
}

/// Build a buffered (single-write) static response. Used by the small
/// branch of `serve_static_hit` and by the streaming path's fallback
/// when a disk insert fails.
///
/// Honours single `Range:` requests by slicing `bytes` (cheap — `Bytes`
/// is refcounted, so a `slice()` is a view, not a copy). Multi-range
/// degrades gracefully to a 200 + full body.
///
/// All sizes (Content-Length, Content-Range total) reflect the
/// VARIANT being served, not identity. ETag is per-variant too —
/// served bytes change with `Accept-Encoding`, so the cache identity
/// must change too.
fn build_buffered_response(
    req: &HttpRequest,
    hit: &crate::dispatch::StaticHit,
    chosen: &ChosenVariant,
    etag: &str,
    bytes: &bytes::Bytes,
    wall_start: std::time::Instant,
) -> HttpResponse {
    // Range handling — resolve before building the response so we set
    // the right status (200 vs 206 vs 416) and Content-Range header.
    let range = parse_range(req.headers().get("range"), chosen.size);
    match range {
        Some(RangeSpec::Unsatisfiable) => return build_range_not_satisfiable(chosen.size, etag),
        Some(RangeSpec::Single(start, end)) => {
            let slice = bytes.slice(start as usize..=end as usize);
            let mut resp = HttpResponse::PartialContent();
            resp.content_type(hit.content_type.clone());
            resp.header("etag", etag);
            resp.header("cache-control", cache_control_header(&hit.cache));
            resp.header(
                "content-range",
                format!("bytes {start}-{end}/{}", chosen.size),
            );
            resp.header("accept-ranges", "bytes");
            apply_variant_headers(&mut resp, chosen);
            resp.header(
                "x-wall-time-ms",
                format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
            );
            return resp.body(Bytes::copy_from_slice(&slice));
        }
        // None or MultiRange → fall through to a full 200.
        _ => {}
    }

    let status = hit.status.unwrap_or(200);
    let st = ntex::http::StatusCode::from_u16(status).unwrap_or(ntex::http::StatusCode::OK);
    let mut resp = HttpResponse::build(st);
    resp.content_type(hit.content_type.clone());
    resp.header("etag", etag);
    resp.header("cache-control", cache_control_header(&hit.cache));
    resp.header("accept-ranges", "bytes");
    apply_variant_headers(&mut resp, chosen);
    resp.header(
        "x-wall-time-ms",
        format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
    );
    // bytes is either an `Arc<Vec<u8>>` (memory tier) or backed by an
    // mmap (disk tier via `Bytes::from_owner`). Either way ntex needs
    // its own `ntex_bytes::Bytes`; the conversion is one userspace
    // copy today, replaced by `sendfile(2)` in Phase C for streamable
    // sizes (large blobs already take the streaming path).
    resp.body(Bytes::copy_from_slice(bytes))
}

/// Build the `Cache-Control` header value from a [`CacheCtl`].
///
/// Emitted directives:
/// * `public, max-age=<n>`    — always
/// * `stale-while-revalidate=<n>`  — when `swr_window` set (RFC 5861)
/// * `stale-if-error=<n>`     — when `stale_on_error` AND `swr_window` (RFC 5861)
/// * `immutable`              — when `immutable: true`
///
/// `background_refresh` is intentionally NOT translated into a
/// Cache-Control directive — it's gateway-internal logic ("re-fetch
/// in the background after max-age expires") rather than a thing
/// browsers / proxies act on. See `serve_static_hit` for the TODO.
fn cache_control_header(c: &zeroship_core::types::CacheCtl) -> String {
    let mut parts: Vec<String> = vec!["public".into(), format!("max-age={}", c.max_age)];
    if let Some(swr) = c.swr_window {
        parts.push(format!("stale-while-revalidate={swr}"));
        if c.stale_on_error {
            // RFC 5861: `stale-if-error` shares the same delta-seconds
            // window as `stale-while-revalidate` for the common case.
            parts.push(format!("stale-if-error={swr}"));
        }
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

    use crate::blob_cache::{BlobCache, DiskBlobCache};
    use zeroship_core::blob::{BlobError, BlobStore};

    /// Build a disk cache rooted in a fresh tmpdir with a generous
    /// budget. Caller is responsible for cleanup (we keep tests
    /// self-contained — the OS will reclaim tmp on reboot if a panic
    /// short-circuits us).
    fn fresh_disk_cache(tag: &str) -> (DiskBlobCache, PathBuf) {
        fresh_disk_cache_with_budget(tag, 1024 * 1024)
    }

    /// Same, but with a configurable byte budget. The streaming-path
    /// tests blow well past 1 MiB so they need a bigger cache.
    fn fresh_disk_cache_with_budget(tag: &str, budget: u64) -> (DiskBlobCache, PathBuf) {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "zsgate-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let cache = DiskBlobCache::new(p.clone(), budget).expect("disk cache");
        (cache, p)
    }

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
        let mem = BlobCache::new(1024);
        let (disk, root) = fresh_disk_cache("miss-fallthrough");
        let store = MockBlobStore::new();
        let hash = "h1";
        store.put(hash, b"hello world");

        let r = fetch_static_bytes(&mem, &disk, &store, hash).await;
        match r {
            BlobFetch::Hit(b) => assert_eq!(&b[..], b"hello world"),
            other => panic!("expected Hit, got {other:?}"),
        }
        assert_eq!(store.calls_for(hash), 1);
        // Both tiers were filled — second call must NOT reach the store.
        let r = fetch_static_bytes(&mem, &disk, &store, hash).await;
        assert!(matches!(r, BlobFetch::Hit(_)));
        assert_eq!(store.calls_for(hash), 1, "second hit must come from cache");
        assert_eq!(mem.len(), 1, "mem tier filled");
        assert_eq!(disk.len(), 1, "disk tier filled");

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn missing_blob_yields_not_found() {
        let mem = BlobCache::new(1024);
        let (disk, root) = fresh_disk_cache("not-found");
        let store = MockBlobStore::new();
        let r = fetch_static_bytes(&mem, &disk, &store, "nope").await;
        assert!(matches!(r, BlobFetch::NotFound));
        // Cache must stay empty on NotFound — otherwise a transient deploy
        // race would poison the cache.
        assert_eq!(mem.len(), 0);
        assert_eq!(disk.len(), 0);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn store_error_yields_unavailable_and_does_not_cache() {
        let mem = BlobCache::new(1024);
        let (disk, root) = fresh_disk_cache("store-err");
        let store = MockBlobStore::new();
        store.put("h1", b"abc");
        store.set_unavailable(true);
        let r = fetch_static_bytes(&mem, &disk, &store, "h1").await;
        assert!(matches!(r, BlobFetch::Unavailable(_)));
        assert_eq!(mem.len(), 0);
        assert_eq!(disk.len(), 0);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn mem_miss_disk_hit_uses_mmap_and_skips_backend() {
        // Pre-fill the disk tier; clear mem; verify the next fetch
        // goes through mmap and never touches the backend.
        let mem = BlobCache::new(1024);
        let (disk, root) = fresh_disk_cache("disk-hit");
        let store = MockBlobStore::new();
        let hash = "deadbeef";
        let payload = b"served from mmap";
        // Manually pre-load the disk cache (simulates a prior fetch
        // that wrote to disk and then aged out of memory).
        disk.insert(hash, payload).expect("disk insert");
        assert_eq!(mem.len(), 0);
        assert_eq!(disk.len(), 1);

        let r = fetch_static_bytes(&mem, &disk, &store, hash).await;
        match r {
            BlobFetch::Hit(b) => assert_eq!(&b[..], payload),
            other => panic!("expected Hit, got {other:?}"),
        }
        assert_eq!(
            store.calls_for(hash),
            0,
            "backend must NOT be called when disk has the blob"
        );
        // The mmap tier promoted into memory on serve.
        assert_eq!(mem.len(), 1, "mem tier filled from disk hit");

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn mem_and_disk_miss_fills_both_tiers() {
        // Cold-cold case: nothing in either tier. The backend serves
        // the bytes; both tiers fill so the second hit short-circuits.
        let mem = BlobCache::new(1024);
        let (disk, root) = fresh_disk_cache("cold-cold");
        let store = MockBlobStore::new();
        let hash = "abc12345";
        store.put(hash, b"backend served");

        let r = fetch_static_bytes(&mem, &disk, &store, hash).await;
        assert!(matches!(r, BlobFetch::Hit(_)));
        assert_eq!(store.calls_for(hash), 1, "first call hits backend");
        assert_eq!(mem.len(), 1, "mem tier filled on miss");
        assert_eq!(disk.len(), 1, "disk tier filled on miss");

        // Verify the disk tier path actually exists on disk.
        let path = disk.local_path(hash).expect("disk entry");
        assert!(path.exists(), "disk file written");
        assert_eq!(std::fs::read(&path).unwrap(), b"backend served");

        std::fs::remove_dir_all(&root).ok();
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

    // -----------------------------------------------------------------------
    // Phase 6 — streaming-body tests
    // -----------------------------------------------------------------------

    use ntex::http::body::{Body, BodySize, MessageBody, ResponseBody};

    fn static_hit(hash: &str, size: u64) -> crate::dispatch::StaticHit {
        crate::dispatch::StaticHit {
            path: "/big.bin".into(),
            hash: hash.into(),
            content_type: "application/octet-stream".into(),
            size,
            cache: zeroship_core::types::CacheCtl {
                max_age: 60,
                swr_window: None,
                immutable: false,
                background_refresh: false,
                stale_on_error: false,
            },
            status: None,
            mutable: false,
            variants: HashMap::new(),
        }
    }

    /// Bare HttpRequest — no headers — for tests that don't care about
    /// conditional-GET / Range parsing. The serve path reads
    /// `if-none-match`, `range`, and `accept-encoding`; an empty
    /// header bag exercises the "no special headers" code path.
    fn bare_request() -> HttpRequest {
        ntex::web::test::TestRequest::default().to_http_request()
    }

    /// Return a 64-char hex hash string for tests. The disk cache
    /// shards by the first two chars; using a real-shaped hash
    /// exercises that path.
    fn hex_hash(byte: u8) -> String {
        let mut s = format!("{byte:02x}");
        s.push_str(&"e".repeat(62));
        s
    }

    /// Drain the chunk stream into a Vec of `Bytes` via the public
    /// Stream interface (mirrors how ntex's body writer would consume
    /// it). Closes the receiver when the senders drop, so a sender
    /// task that exits cleanly terminates the loop.
    async fn drain_chunks(
        rx: ntex::channel::mpsc::Receiver<Result<Bytes, Rc<dyn std::error::Error>>>,
    ) -> Vec<Bytes> {
        let mut out = Vec::new();
        loop {
            match rx.recv().await {
                Some(Ok(b)) => out.push(b),
                Some(Err(e)) => panic!("chunk stream error: {e}"),
                None => return out,
            }
        }
    }

    #[compio::test]
    async fn chunk_reader_emits_full_chunks_then_remainder() {
        // 200 bytes of payload, 64-byte chunks → expect 64+64+64+8.
        let payload: Vec<u8> = (0..200).map(|i| i as u8).collect();
        let (tx, rx) = ntex::channel::mpsc::channel::<Result<Bytes, Rc<dyn std::error::Error>>>();
        chunk_stream_from_file(&payload, payload.len() as u64, 64, &tx).await;
        drop(tx);

        let chunks = drain_chunks(rx).await;
        assert_eq!(chunks.len(), 4, "200 bytes / 64-byte chunks → 4 chunks");
        assert_eq!(chunks[0].len(), 64);
        assert_eq!(chunks[1].len(), 64);
        assert_eq!(chunks[2].len(), 64);
        assert_eq!(chunks[3].len(), 8, "trailing partial chunk");

        // Concatenated chunks must reproduce the source byte-for-byte.
        let mut joined = Vec::with_capacity(payload.len());
        for c in &chunks {
            joined.extend_from_slice(c);
        }
        assert_eq!(joined, payload, "stream output reassembles to source");
    }

    #[compio::test]
    async fn chunk_reader_handles_size_smaller_than_chunk() {
        // Edge case: payload smaller than one chunk → single short chunk.
        let payload = vec![7u8; 100];
        let (tx, rx) = ntex::channel::mpsc::channel::<Result<Bytes, Rc<dyn std::error::Error>>>();
        chunk_stream_from_file(&payload, payload.len() as u64, 64 * 1024, &tx).await;
        drop(tx);

        let chunks = drain_chunks(rx).await;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 100);
        assert_eq!(&chunks[0][..], payload.as_slice());
    }

    #[compio::test]
    async fn chunk_stream_from_path_reads_real_file_chunk_by_chunk() {
        // End-to-end: write a file under a fresh tmpdir, stream it,
        // assert chunks come back in order and recombine to the source.
        let mut dir = std::env::temp_dir();
        dir.push(format!("zsstream-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("payload.bin");
        // 5 * 1 KiB so we get multiple chunks of 1 KiB plus a small
        // tail when chunk_bytes = 1024.
        let payload: Vec<u8> = (0..5 * 1024 + 7).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &payload).expect("write");

        let rx = chunk_stream_from_path(path.clone(), payload.len() as u64, 1024);
        let chunks = drain_chunks(rx).await;

        let total: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(total, payload.len(), "total streamed = file size");
        // Most chunks should be 1024 bytes; the tail one is whatever's
        // left. We don't assert exact chunk count because compio is
        // free to short-read on a single-syscall basis.
        assert!(chunks.len() >= 5, "got at least 5 chunks");
        let mut joined = Vec::with_capacity(payload.len());
        for c in &chunks {
            joined.extend_from_slice(c);
        }
        assert_eq!(joined, payload, "round-trip identity");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[compio::test]
    async fn ensure_disk_path_uses_existing_disk_entry() {
        let (disk, root) = fresh_disk_cache("ensure-existing");
        let store = MockBlobStore::new();
        let hash = hex_hash(0x10);
        let payload = vec![0u8; 4096];
        // Pre-fill disk so ensure_disk_path returns OnDisk without
        // calling the backend.
        disk.insert(&hash, &payload).expect("insert");

        match ensure_disk_path(&disk, &store, &hash).await {
            DiskAvailability::OnDisk(p) => assert!(p.exists(), "real path"),
            other => panic!("expected OnDisk, got {other:?}"),
        }
        assert_eq!(store.calls_for(&hash), 0, "disk hit must not call backend");

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn ensure_disk_path_fetches_backend_and_fills_disk() {
        // Generous budget so the multi-MB payload actually lands on
        // disk (the default 1 MiB budget would no-op the insert).
        let (disk, root) = fresh_disk_cache_with_budget("ensure-cold", 8 * 1024 * 1024);
        let store = MockBlobStore::new();
        let hash = hex_hash(0x20);
        let payload: Vec<u8> = (0..(STREAM_THRESHOLD_BYTES + 4096) as usize)
            .map(|i| i as u8)
            .collect();
        store.put(&hash, &payload);

        match ensure_disk_path(&disk, &store, &hash).await {
            DiskAvailability::OnDisk(p) => {
                assert!(p.exists(), "backend fill wrote the file");
                let on_disk = std::fs::read(&p).expect("read back");
                assert_eq!(on_disk.len(), payload.len(), "size matches");
            }
            other => panic!("expected OnDisk, got {other:?}"),
        }
        assert_eq!(store.calls_for(&hash), 1, "exactly one backend call");

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn ensure_disk_path_propagates_not_found() {
        let (disk, root) = fresh_disk_cache("ensure-404");
        let store = MockBlobStore::new();
        match ensure_disk_path(&disk, &store, &hex_hash(0x99)).await {
            DiskAvailability::NotFound => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn ensure_disk_path_propagates_unavailable() {
        let (disk, root) = fresh_disk_cache("ensure-503");
        let store = MockBlobStore::new();
        let hash = hex_hash(0x33);
        store.put(&hash, b"x");
        store.set_unavailable(true);
        match ensure_disk_path(&disk, &store, &hash).await {
            DiskAvailability::Unavailable(_) => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// Build a `GateState` with a swappable blob store. Pulled out so
    /// the `serve_static_*` tests aren't constructing it inline. The
    /// 8 MiB mem-cache budget is generous enough that the buffered
    /// path's insert won't no-op for the test payloads we care about
    /// (the production default is 256 MiB).
    fn make_state(
        store: Arc<dyn zeroship_core::blob::BlobStore>,
        disk: crate::blob_cache::DiskBlobCache,
    ) -> GateState {
        GateState {
            config: crate::GateConfig {
                control_url: String::new(),
                control_key: String::new(),
                worker_urls: vec![],
                poll_interval_secs: 5,
                auth_secret: String::new(),
                worker_key: String::new(),
            },
            routes: crate::sync::RouteCache::new(),
            hash_ring: crate::proxy::HashRing::new(vec!["http://0.0.0.0:0".into()], 1),
            rate_limiters: crate::enforce::RateLimitRegistry::new(1, 1),
            per_rule_rate_limits: crate::enforce::PerRuleRateLimitRegistry::new(),
            concurrency: crate::enforce::ConcurrencyRegistry::new(1),
            blob_store: store,
            blob_cache: BlobCache::new(8 * 1024 * 1024),
            disk_cache: disk,
        }
    }

    /// Adapter shim — a `MockBlobStore` lives behind an `Arc` for
    /// `GateState`, but the test still needs a non-Arc handle to
    /// inspect call counts after the fact.
    struct MockHandle {
        inner: Arc<MockBlobStore>,
    }

    impl MockHandle {
        fn new() -> Self {
            Self { inner: Arc::new(MockBlobStore::new()) }
        }
        fn put(&self, hash: &str, data: &[u8]) {
            self.inner.put(hash, data);
        }
        fn calls_for(&self, hash: &str) -> usize {
            self.inner.calls_for(hash)
        }
        fn store(&self) -> Arc<dyn zeroship_core::blob::BlobStore> {
            self.inner.clone()
        }
    }

    /// Drain a `ResponseBody<Body>` to completion, returning the
    /// concatenated payload. Mirrors what ntex would do on the wire,
    /// minus the actual socket write.
    async fn collect_body(mut body: ResponseBody<Body>) -> Vec<u8> {
        let mut out = Vec::new();
        std::future::poll_fn(|cx| {
            loop {
                match body.poll_next_chunk(cx) {
                    std::task::Poll::Ready(Some(Ok(chunk))) => {
                        out.extend_from_slice(&chunk);
                    }
                    std::task::Poll::Ready(Some(Err(e))) => panic!("body error: {e}"),
                    std::task::Poll::Ready(None) => return std::task::Poll::Ready(()),
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                }
            }
        })
        .await;
        out
    }

    #[compio::test]
    async fn small_blob_uses_buffered_path() {
        // Threshold is 1 MiB. A 100 KB blob must take the buffered
        // path → Body::Bytes → BodySize::Sized(100K).
        let (disk, root) = fresh_disk_cache("small-buffered");
        let mock = MockHandle::new();
        let hash = hex_hash(0x42);
        let payload = vec![0xABu8; 100 * 1024];
        mock.put(&hash, &payload);

        let state = make_state(mock.store(), disk);
        let hit = static_hit(&hash, payload.len() as u64);
        let req = bare_request();
        let mut resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;

        assert_eq!(resp.status(), ntex::http::StatusCode::OK);
        let body = resp.take_body();
        // Buffered path stores the bytes inline in `Body::Bytes`,
        // never wraps in `Body::Message`. That's what distinguishes
        // it from the streaming path on the wire.
        assert!(matches!(
            &body,
            ResponseBody::Body(Body::Bytes(_)) | ResponseBody::Other(Body::Bytes(_))
        ), "small blob must produce Body::Bytes");
        assert_eq!(body.size(), BodySize::Sized(payload.len() as u64));
        let got = collect_body(body).await;
        assert_eq!(got, payload);
        // Mem cache filled — the small path still benefits from
        // refcounted sharing across requests.
        assert_eq!(state.blob_cache.len(), 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn large_blob_uses_streaming_path_and_skips_mem_cache() {
        // 5 MiB blob → above the 1 MiB threshold → streaming path.
        let (disk, root) = fresh_disk_cache_with_budget("large-streaming", 32 * 1024 * 1024);
        let mock = MockHandle::new();
        let hash = hex_hash(0x55);
        let size: usize = 5 * 1024 * 1024 + 13;
        let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        mock.put(&hash, &payload);

        let state = make_state(mock.store(), disk);
        let hit = static_hit(&hash, payload.len() as u64);
        let req = bare_request();
        let mut resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;

        assert_eq!(resp.status(), ntex::http::StatusCode::OK);
        let body = resp.take_body();
        // Streaming path → Body::Message with BodySize::Sized.
        assert!(matches!(
            &body,
            ResponseBody::Body(Body::Message(_)) | ResponseBody::Other(Body::Message(_))
        ), "large blob must produce a streaming Body::Message");
        assert_eq!(body.size(), BodySize::Sized(payload.len() as u64));
        let got = collect_body(body).await;
        assert_eq!(got.len(), payload.len(), "streamed length matches");
        assert_eq!(got, payload, "streamed bytes match source");

        // Mem cache MUST be empty — the streaming path skips it so a
        // 5 MB blob doesn't blow out the budget for everything else.
        assert_eq!(
            state.blob_cache.len(),
            0,
            "streaming path must not insert into mem cache"
        );
        // Disk cache filled on the cold-path miss → subsequent serves
        // skip the backend.
        assert_eq!(state.disk_cache.len(), 1);
        // Backend was called exactly once for the cold fill.
        assert_eq!(mock.calls_for(&hash), 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn large_blob_streams_from_warm_disk_without_backend() {
        // Pre-fill disk; verify the streaming path never calls the
        // backend on the warm path.
        let (disk, root) = fresh_disk_cache_with_budget("large-warm-disk", 8 * 1024 * 1024);
        let mock = MockHandle::new();
        let hash = hex_hash(0x66);
        let size: usize = 2 * 1024 * 1024;
        let payload = vec![0xCDu8; size];
        // Fill disk only — backend MUST NOT be consulted.
        disk.insert(&hash, &payload).expect("disk insert");

        let state = make_state(mock.store(), disk);
        let hit = static_hit(&hash, payload.len() as u64);
        let req = bare_request();
        let mut resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;

        assert_eq!(resp.status(), ntex::http::StatusCode::OK);
        let body = resp.take_body();
        assert_eq!(body.size(), BodySize::Sized(payload.len() as u64));
        let got = collect_body(body).await;
        assert_eq!(got, payload);
        assert_eq!(
            mock.calls_for(&hash),
            0,
            "warm disk + streaming path must not hit backend"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn large_blob_not_found_returns_404() {
        let (disk, root) = fresh_disk_cache("large-404");
        let mock = MockHandle::new();
        let state = make_state(mock.store(), disk);
        let hit = static_hit(&hex_hash(0x77), STREAM_THRESHOLD_BYTES + 1);
        let req = bare_request();
        let resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::NOT_FOUND);

        std::fs::remove_dir_all(&root).ok();
    }

    impl std::fmt::Debug for DiskAvailability {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::OnDisk(p) => f.debug_tuple("OnDisk").field(p).finish(),
                Self::InMemoryOnly(b) => f.debug_tuple("InMemoryOnly").field(&b.len()).finish(),
                Self::NotFound => f.write_str("NotFound"),
                Self::Unavailable(s) => f.debug_tuple("Unavailable").field(s).finish(),
            }
        }
    }

    // -----------------------------------------------------------------------
    // CORS tests — preflight + response-header injection
    // -----------------------------------------------------------------------

    use crate::compiled::CompiledManifest;
    use zeroship_core::types::{Cors, HttpMethod, Manifest, ResourceEntry};

    /// Single-resource manifest with a CORS policy attached to `/api/*`.
    /// All preflight tests use this shape; the path narrowness keeps
    /// the assertions specific.
    fn manifest_with_cors(cors: Cors) -> Manifest {
        let mut resources = std::collections::HashMap::new();
        resources.insert(
            "/api/*".into(),
            ResourceEntry {
                cors: Some(cors),
                ..Default::default()
            },
        );
        Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        }
    }

    fn header_str<'a>(resp: &'a HttpResponse, name: &str) -> Option<&'a str> {
        resp.headers().get(name).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn preflight_allowed_origin() {
        let cors = Cors {
            allow_origins: vec!["https://example.com".into()],
            allow_methods: vec![HttpMethod::Get, HttpMethod::Post],
            allow_headers: vec!["content-type".into(), "authorization".into()],
            expose_headers: vec![],
            allow_credentials: false,
            max_age_seconds: Some(600),
        };
        let m = manifest_with_cors(cors.clone());
        let c = CompiledManifest::compile(&m);
        // Preflight asks "can I POST /api/users?" — the matched
        // resource carries CORS so we answer 204.
        let policy = c
            .lookup_resource("/api/users")
            .expect("matches /api/* resource");
        let policy_cors = policy.cors.as_ref().expect("resource has cors policy");
        assert_eq!(policy_cors.allow_origins, cors.allow_origins);
        let resp = build_preflight_response(
            policy_cors,
            "https://example.com",
            std::time::Instant::now(),
        );
        assert_eq!(resp.status(), ntex::http::StatusCode::NO_CONTENT);
        assert_eq!(
            header_str(&resp, "access-control-allow-origin"),
            Some("https://example.com")
        );
        assert_eq!(header_str(&resp, "vary"), Some("Origin"));
        let methods = header_str(&resp, "access-control-allow-methods").unwrap();
        assert!(methods.contains("GET"));
        assert!(methods.contains("POST"));
        let headers = header_str(&resp, "access-control-allow-headers").unwrap();
        assert!(headers.contains("content-type"));
        assert_eq!(header_str(&resp, "access-control-max-age"), Some("600"));
    }

    #[test]
    fn preflight_disallowed_origin() {
        // Origin is not in the allow list — respond 204 but WITHOUT
        // `Access-Control-Allow-Origin`. The browser blocks.
        let cors = Cors {
            allow_origins: vec!["https://allowed.com".into()],
            allow_methods: vec![HttpMethod::Post],
            allow_headers: vec![],
            expose_headers: vec![],
            allow_credentials: false,
            max_age_seconds: None,
        };
        let resp = build_preflight_response(
            &cors,
            "https://evil.com",
            std::time::Instant::now(),
        );
        assert_eq!(resp.status(), ntex::http::StatusCode::NO_CONTENT);
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "no allow-origin header for disallowed origin"
        );
        assert!(resp.headers().get("vary").is_none());
    }

    #[test]
    fn preflight_wildcard_origin_no_credentials() {
        let cors = Cors {
            allow_origins: vec!["*".into()],
            allow_methods: vec![HttpMethod::Get],
            allow_headers: vec![],
            expose_headers: vec![],
            allow_credentials: false,
            max_age_seconds: None,
        };
        let resp = build_preflight_response(
            &cors,
            "https://anything.example",
            std::time::Instant::now(),
        );
        assert_eq!(
            header_str(&resp, "access-control-allow-origin"),
            Some("*")
        );
        // No `Vary` for wildcard responses — they don't depend on origin.
        assert!(resp.headers().get("vary").is_none());
        assert!(
            resp.headers()
                .get("access-control-allow-credentials")
                .is_none(),
            "no credentials header on wildcard preflight"
        );
    }

    #[test]
    fn preflight_no_matching_resource_falls_through() {
        // Manifest has a CORS resource at /api/*, but the request
        // targets /other. lookup_resource returns None → router falls
        // through to a 404 response.
        let cors = Cors {
            allow_origins: vec!["https://example.com".into()],
            allow_methods: vec![HttpMethod::Post],
            allow_headers: vec![],
            expose_headers: vec![],
            allow_credentials: false,
            max_age_seconds: None,
        };
        let m = manifest_with_cors(cors);
        let c = CompiledManifest::compile(&m);
        assert!(c.lookup_resource("/other").is_none());
    }

    #[test]
    fn actual_request_injects_cors_headers() {
        // Non-preflight: real POST with Origin matching → response
        // headers include allow-origin + Vary.
        let cors = Cors {
            allow_origins: vec!["https://example.com".into()],
            allow_methods: vec![HttpMethod::Post],
            allow_headers: vec![],
            expose_headers: vec!["x-request-id".into()],
            allow_credentials: false,
            max_age_seconds: None,
        };
        let mut resp = HttpResponse::Ok().finish();
        inject_cors_response_headers(resp.headers_mut(), &cors, "https://example.com");
        assert_eq!(
            header_str(&resp, "access-control-allow-origin"),
            Some("https://example.com")
        );
        assert_eq!(header_str(&resp, "vary"), Some("Origin"));
        assert_eq!(
            header_str(&resp, "access-control-expose-headers"),
            Some("x-request-id")
        );
    }

    #[test]
    fn actual_request_disallowed_origin_no_headers() {
        let cors = Cors {
            allow_origins: vec!["https://allowed.com".into()],
            allow_methods: vec![HttpMethod::Post],
            allow_headers: vec![],
            expose_headers: vec!["x-request-id".into()],
            allow_credentials: false,
            max_age_seconds: None,
        };
        let mut resp = HttpResponse::Ok().finish();
        inject_cors_response_headers(resp.headers_mut(), &cors, "https://evil.com");
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "no allow-origin for disallowed origin"
        );
        assert!(resp.headers().get("vary").is_none());
        assert!(
            resp.headers()
                .get("access-control-expose-headers")
                .is_none(),
            "expose-headers requires an allowed origin"
        );
    }

    #[test]
    fn actual_request_wildcard_no_credentials() {
        let cors = Cors {
            allow_origins: vec!["*".into()],
            allow_methods: vec![HttpMethod::Get],
            allow_headers: vec![],
            expose_headers: vec![],
            allow_credentials: false,
            max_age_seconds: None,
        };
        let mut resp = HttpResponse::Ok().finish();
        inject_cors_response_headers(resp.headers_mut(), &cors, "https://anything.example");
        assert_eq!(
            header_str(&resp, "access-control-allow-origin"),
            Some("*")
        );
        assert!(resp.headers().get("vary").is_none(), "no Vary on wildcard");
    }

    // -----------------------------------------------------------------------
    // Tier 4a — HTTP completeness tests
    //   * If-None-Match → 304 (with no blob fetch)
    //   * Range: parsing + 206/416 responses on buffered + streaming paths
    //   * Cache-Control extensions (stale-if-error)
    //   * Accept-Ranges always advertised
    // -----------------------------------------------------------------------

    /// Build a static hit with a real-shaped 64-char hex hash.
    fn hex_static_hit(byte: u8, size: u64) -> crate::dispatch::StaticHit {
        let mut hit = static_hit(&hex_hash(byte), size);
        hit.size = size;
        hit
    }

    /// Helper: extract an owned String for a response header.
    fn hdr(resp: &HttpResponse, name: &str) -> Option<String> {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    // ── etag_matches ────────────────────────────────────────────────────────

    #[test]
    fn etag_matches_exact() {
        assert!(etag_matches("\"abc\"", "\"abc\""));
        assert!(!etag_matches("\"abc\"", "\"xyz\""));
    }

    #[test]
    fn etag_matches_wildcard() {
        assert!(etag_matches("*", "\"abc\""));
        // Wildcard with surrounding whitespace is also valid.
        assert!(etag_matches("  *  ", "\"abc\""));
    }

    #[test]
    fn etag_matches_list() {
        assert!(etag_matches("\"abc\", \"def\"", "\"def\""));
        assert!(etag_matches("\"abc\",\"def\"", "\"abc\""));
        assert!(!etag_matches("\"abc\", \"def\"", "\"xyz\""));
    }

    #[test]
    fn etag_matches_weak_rejected() {
        // Weak ETags must NOT match — strong comparison only.
        assert!(!etag_matches("W/\"abc\"", "\"abc\""));
        // Mixed strong + weak in a list — only the strong entries
        // can match.
        assert!(etag_matches("W/\"abc\", \"def\"", "\"def\""));
        assert!(!etag_matches("W/\"abc\", W/\"def\"", "\"def\""));
    }

    // ── parse_range ─────────────────────────────────────────────────────────

    fn range_header(s: &str) -> ntex::http::header::HeaderValue {
        ntex::http::header::HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn parse_range_single() {
        let h = range_header("bytes=0-9");
        assert_eq!(parse_range(Some(&h), 100), Some(RangeSpec::Single(0, 9)));
    }

    #[test]
    fn parse_range_open_end() {
        let h = range_header("bytes=10-");
        assert_eq!(parse_range(Some(&h), 100), Some(RangeSpec::Single(10, 99)));
    }

    #[test]
    fn parse_range_suffix() {
        let h = range_header("bytes=-20");
        assert_eq!(parse_range(Some(&h), 100), Some(RangeSpec::Single(80, 99)));
        // Suffix bigger than file → clamp to whole file.
        let h2 = range_header("bytes=-500");
        assert_eq!(parse_range(Some(&h2), 100), Some(RangeSpec::Single(0, 99)));
    }

    #[test]
    fn parse_range_clamps_end_to_size_minus_one() {
        let h = range_header("bytes=50-9999");
        assert_eq!(parse_range(Some(&h), 100), Some(RangeSpec::Single(50, 99)));
    }

    #[test]
    fn parse_range_unsatisfiable() {
        let h = range_header("bytes=200-300");
        assert_eq!(parse_range(Some(&h), 100), Some(RangeSpec::Unsatisfiable));
        // start > end with start in range → still unsatisfiable.
        let h2 = range_header("bytes=99-50");
        assert_eq!(parse_range(Some(&h2), 100), Some(RangeSpec::Unsatisfiable));
    }

    #[test]
    fn parse_range_multi() {
        let h = range_header("bytes=0-10,20-30");
        assert_eq!(parse_range(Some(&h), 100), Some(RangeSpec::MultiRange));
    }

    #[test]
    fn parse_range_unknown_unit() {
        let h = range_header("items=1-2");
        assert_eq!(parse_range(Some(&h), 100), None);
    }

    #[test]
    fn parse_range_garbage() {
        let h = range_header("bytes=abc");
        assert_eq!(parse_range(Some(&h), 100), None);
    }

    #[test]
    fn parse_range_absent() {
        assert_eq!(parse_range(None, 100), None);
    }

    // ── If-None-Match → 304 ─────────────────────────────────────────────────

    #[compio::test]
    async fn if_none_match_returns_304_without_blob_fetch() {
        let (disk, root) = fresh_disk_cache("inm-304");
        let mock = MockHandle::new();
        let hash = hex_hash(0x10);
        // Note: NOT putting the blob in the store. Conditional GET
        // must short-circuit BEFORE the fetch even tries.
        let state = make_state(mock.store(), disk);
        let hit = hex_static_hit(0x10, 1024);

        let req = ntex::web::test::TestRequest::default()
            .header("if-none-match", format!("\"{}\"", hash))
            .to_http_request();
        let resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;

        assert_eq!(resp.status(), ntex::http::StatusCode::NOT_MODIFIED);
        // ETag, Cache-Control, Accept-Ranges all present on the 304.
        assert!(hdr(&resp, "etag").is_some(), "etag on 304");
        assert!(hdr(&resp, "cache-control").is_some(), "cache-control on 304");
        assert_eq!(hdr(&resp, "accept-ranges").as_deref(), Some("bytes"));
        // Critical: backend was NOT called.
        assert_eq!(mock.calls_for(&hash), 0, "no blob fetch on 304");

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn if_none_match_wildcard_matches() {
        let (disk, root) = fresh_disk_cache("inm-wildcard");
        let mock = MockHandle::new();
        let hash = hex_hash(0x11);
        let state = make_state(mock.store(), disk);
        let hit = hex_static_hit(0x11, 1024);

        let req = ntex::web::test::TestRequest::default()
            .header("if-none-match", "*")
            .to_http_request();
        let resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::NOT_MODIFIED);
        assert_eq!(mock.calls_for(&hash), 0);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn if_none_match_list_matches() {
        let (disk, root) = fresh_disk_cache("inm-list");
        let mock = MockHandle::new();
        let hash = hex_hash(0x12);
        let state = make_state(mock.store(), disk);
        let hit = hex_static_hit(0x12, 1024);

        // List with the matching ETag in the middle.
        let req = ntex::web::test::TestRequest::default()
            .header("if-none-match", format!("\"abc\", \"{}\", \"def\"", hash))
            .to_http_request();
        let resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::NOT_MODIFIED);
        assert_eq!(mock.calls_for(&hash), 0);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn if_none_match_weak_etag_rejected() {
        // W/"<hash>" must NOT short-circuit. Caller must fetch the blob
        // and respond 200.
        let (disk, root) = fresh_disk_cache("inm-weak");
        let mock = MockHandle::new();
        let hash = hex_hash(0x13);
        let payload = vec![0xAAu8; 1024];
        mock.put(&hash, &payload);
        let state = make_state(mock.store(), disk);
        let hit = hex_static_hit(0x13, payload.len() as u64);

        let req = ntex::web::test::TestRequest::default()
            .header("if-none-match", format!("W/\"{}\"", hash))
            .to_http_request();
        let resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::OK);
        assert_eq!(mock.calls_for(&hash), 1, "weak match must fetch the blob");

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Range — buffered path ───────────────────────────────────────────────

    #[compio::test]
    async fn range_serves_partial_content() {
        let (disk, root) = fresh_disk_cache("range-206");
        let mock = MockHandle::new();
        let hash = hex_hash(0x20);
        let payload: Vec<u8> = (0..100).collect();
        mock.put(&hash, &payload);
        let state = make_state(mock.store(), disk);
        let hit = hex_static_hit(0x20, payload.len() as u64);

        let req = ntex::web::test::TestRequest::default()
            .header("range", "bytes=0-9")
            .to_http_request();
        let mut resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(hdr(&resp, "content-range").as_deref(), Some("bytes 0-9/100"));
        assert_eq!(hdr(&resp, "accept-ranges").as_deref(), Some("bytes"));
        let body = resp.take_body();
        let got = collect_body(body).await;
        assert_eq!(got, payload[0..10]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn range_open_end() {
        let (disk, root) = fresh_disk_cache("range-open");
        let mock = MockHandle::new();
        let hash = hex_hash(0x21);
        let payload: Vec<u8> = (0..100).collect();
        mock.put(&hash, &payload);
        let state = make_state(mock.store(), disk);
        let hit = hex_static_hit(0x21, payload.len() as u64);

        let req = ntex::web::test::TestRequest::default()
            .header("range", "bytes=10-")
            .to_http_request();
        let mut resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(hdr(&resp, "content-range").as_deref(), Some("bytes 10-99/100"));
        let got = collect_body(resp.take_body()).await;
        assert_eq!(got, payload[10..]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn range_suffix() {
        let (disk, root) = fresh_disk_cache("range-suffix");
        let mock = MockHandle::new();
        let hash = hex_hash(0x22);
        let payload: Vec<u8> = (0..100).collect();
        mock.put(&hash, &payload);
        let state = make_state(mock.store(), disk);
        let hit = hex_static_hit(0x22, payload.len() as u64);

        let req = ntex::web::test::TestRequest::default()
            .header("range", "bytes=-20")
            .to_http_request();
        let mut resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(hdr(&resp, "content-range").as_deref(), Some("bytes 80-99/100"));
        let got = collect_body(resp.take_body()).await;
        assert_eq!(got, payload[80..]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn range_unsatisfiable_returns_416() {
        let (disk, root) = fresh_disk_cache("range-416");
        let mock = MockHandle::new();
        let hash = hex_hash(0x23);
        let payload: Vec<u8> = (0..100).collect();
        mock.put(&hash, &payload);
        let state = make_state(mock.store(), disk);
        let hit = hex_static_hit(0x23, payload.len() as u64);

        let req = ntex::web::test::TestRequest::default()
            .header("range", "bytes=200-300")
            .to_http_request();
        let resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(hdr(&resp, "content-range").as_deref(), Some("bytes */100"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn range_multi_falls_through_to_200() {
        let (disk, root) = fresh_disk_cache("range-multi");
        let mock = MockHandle::new();
        let hash = hex_hash(0x24);
        let payload: Vec<u8> = (0..100).collect();
        mock.put(&hash, &payload);
        let state = make_state(mock.store(), disk);
        let hit = hex_static_hit(0x24, payload.len() as u64);

        let req = ntex::web::test::TestRequest::default()
            .header("range", "bytes=0-10,20-30")
            .to_http_request();
        let mut resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::OK, "multi-range degrades to 200");
        let got = collect_body(resp.take_body()).await;
        assert_eq!(got, payload, "full body served on multi-range");

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Range — streaming path ──────────────────────────────────────────────

    #[compio::test]
    async fn range_on_streaming_path() {
        // Asset over the streaming threshold; range request slices it.
        let (disk, root) = fresh_disk_cache_with_budget("range-streaming", 8 * 1024 * 1024);
        let mock = MockHandle::new();
        let hash = hex_hash(0x30);
        let size: usize = (STREAM_THRESHOLD_BYTES + 1024) as usize;
        let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        mock.put(&hash, &payload);
        let state = make_state(mock.store(), disk);
        let hit = hex_static_hit(0x30, payload.len() as u64);

        let req = ntex::web::test::TestRequest::default()
            .header("range", "bytes=100-199")
            .to_http_request();
        let mut resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            hdr(&resp, "content-range"),
            Some(format!("bytes 100-199/{}", payload.len()))
        );
        let got = collect_body(resp.take_body()).await;
        assert_eq!(got.len(), 100, "exactly 100 bytes streamed");
        assert_eq!(got, payload[100..200], "streamed bytes match the requested slice");

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn chunk_stream_from_file_range_reads_offset_correctly() {
        // Verify the range-aware chunk reader skips to the offset and
        // emits exactly `length` bytes — no overshoot.
        let payload: Vec<u8> = (0..200).map(|i| i as u8).collect();
        let (tx, rx) = ntex::channel::mpsc::channel::<Result<Bytes, Rc<dyn std::error::Error>>>();
        chunk_stream_from_file_range(&payload, 50, 30, 16, &tx).await;
        drop(tx);

        let chunks = drain_chunks(rx).await;
        let mut joined = Vec::new();
        for c in &chunks {
            joined.extend_from_slice(c);
        }
        assert_eq!(joined.len(), 30, "exactly 30 bytes streamed");
        assert_eq!(joined, payload[50..80], "bytes match offset/length");
    }

    // ── cache_control_header ────────────────────────────────────────────────

    #[test]
    fn cache_ctl_emits_stale_if_error() {
        // stale_on_error AND swr_window set → both stale-while-revalidate
        // and stale-if-error directives.
        let c = zeroship_core::types::CacheCtl {
            max_age: 60,
            swr_window: Some(30),
            immutable: false,
            background_refresh: false,
            stale_on_error: true,
        };
        let v = cache_control_header(&c);
        assert!(v.contains("stale-while-revalidate=30"), "swr present: {v}");
        assert!(v.contains("stale-if-error=30"), "stale-if-error present: {v}");
    }

    #[test]
    fn cache_ctl_no_stale_if_error_without_swr() {
        // stale_on_error WITHOUT swr_window → no stale-if-error.
        let c = zeroship_core::types::CacheCtl {
            max_age: 60,
            swr_window: None,
            immutable: false,
            background_refresh: false,
            stale_on_error: true,
        };
        let v = cache_control_header(&c);
        assert!(!v.contains("stale-if-error"), "no swr → no stale-if-error: {v}");
    }

    #[test]
    fn cache_ctl_immutable_still_works() {
        let c = zeroship_core::types::CacheCtl {
            max_age: 31_536_000,
            swr_window: None,
            immutable: true,
            background_refresh: false,
            stale_on_error: false,
        };
        let v = cache_control_header(&c);
        assert!(v.contains("immutable"), "immutable preserved: {v}");
        assert!(v.contains("max-age=31536000"));
    }

    // ── Accept-Ranges always advertised ─────────────────────────────────────

    #[compio::test]
    async fn accept_ranges_header_always_present() {
        // 200 (buffered), 206 (range), 304 (conditional GET) all carry
        // Accept-Ranges so clients know they can range.
        let (disk, root) = fresh_disk_cache("accept-ranges");
        let mock = MockHandle::new();
        let hash = hex_hash(0x40);
        let payload = vec![0xCDu8; 1024];
        mock.put(&hash, &payload);
        let state = make_state(mock.store(), disk);

        // 200 OK
        let hit = hex_static_hit(0x40, payload.len() as u64);
        let req = bare_request();
        let resp200 = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp200.status(), ntex::http::StatusCode::OK);
        assert_eq!(hdr(&resp200, "accept-ranges").as_deref(), Some("bytes"));

        // 206 Partial Content
        let hit = hex_static_hit(0x40, payload.len() as u64);
        let req = ntex::web::test::TestRequest::default()
            .header("range", "bytes=0-9")
            .to_http_request();
        let resp206 = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp206.status(), ntex::http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(hdr(&resp206, "accept-ranges").as_deref(), Some("bytes"));

        // 304 Not Modified
        let hit = hex_static_hit(0x40, payload.len() as u64);
        let req = ntex::web::test::TestRequest::default()
            .header("if-none-match", format!("\"{}\"", hash))
            .to_http_request();
        let resp304 = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp304.status(), ntex::http::StatusCode::NOT_MODIFIED);
        assert_eq!(hdr(&resp304, "accept-ranges").as_deref(), Some("bytes"));

        std::fs::remove_dir_all(&root).ok();
    }

    // ── Streaming path advertises Accept-Ranges too ─────────────────────────

    #[compio::test]
    async fn streaming_path_emits_accept_ranges() {
        let (disk, root) = fresh_disk_cache_with_budget("stream-ar", 8 * 1024 * 1024);
        let mock = MockHandle::new();
        let hash = hex_hash(0x50);
        let size: usize = (STREAM_THRESHOLD_BYTES + 1024) as usize;
        let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        mock.put(&hash, &payload);
        let state = make_state(mock.store(), disk);
        let hit = hex_static_hit(0x50, payload.len() as u64);

        let req = bare_request();
        let resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::OK);
        assert_eq!(hdr(&resp, "accept-ranges").as_deref(), Some("bytes"));

        std::fs::remove_dir_all(&root).ok();
    }

    // -----------------------------------------------------------------------
    // Tier 4b — Accept-Encoding negotiation
    // -----------------------------------------------------------------------

    use zeroship_core::types::AssetVariant;

    /// Build a static hit whose `variants` map carries `br` and `gzip`
    /// entries pointing at the given hashes/sizes. Used by the
    /// negotiation tests below.
    fn static_hit_with_variants(
        identity_byte: u8,
        identity_size: u64,
        variants: HashMap<String, AssetVariant>,
    ) -> crate::dispatch::StaticHit {
        let mut hit = hex_static_hit(identity_byte, identity_size);
        hit.variants = variants;
        hit
    }

    #[test]
    fn pick_variant_no_header_returns_identity() {
        let hit = static_hit_with_variants(
            0x01,
            1024,
            HashMap::from([(
                "br".into(),
                AssetVariant { hash: hex_hash(0xBB), size: 256 },
            )]),
        );
        let chosen = pick_variant(&hit, None);
        assert!(chosen.encoding.is_none(), "no Accept-Encoding → identity");
        assert_eq!(chosen.hash, hit.hash);
        assert_eq!(chosen.size, hit.size);
    }

    #[test]
    fn pick_variant_no_variants_map_returns_identity() {
        let hit = hex_static_hit(0x02, 1024);
        let chosen = pick_variant(&hit, Some("br, gzip"));
        assert!(chosen.encoding.is_none());
        assert_eq!(chosen.hash, hit.hash);
    }

    #[test]
    fn pick_variant_brotli_preferred_over_gzip() {
        let br_hash = hex_hash(0xBB);
        let gz_hash = hex_hash(0x6F);
        let hit = static_hit_with_variants(
            0x03,
            10000,
            HashMap::from([
                ("br".into(), AssetVariant { hash: br_hash.clone(), size: 1500 }),
                ("gzip".into(), AssetVariant { hash: gz_hash.clone(), size: 2500 }),
            ]),
        );
        // Client lists br first → server picks br.
        let chosen = pick_variant(&hit, Some("br, gzip"));
        assert_eq!(chosen.encoding.as_deref(), Some("br"));
        assert_eq!(chosen.hash, br_hash);
        assert_eq!(chosen.size, 1500);
    }

    #[test]
    fn pick_variant_gzip_when_only_gzip_accepted() {
        let gz_hash = hex_hash(0x6F);
        let hit = static_hit_with_variants(
            0x04,
            10000,
            HashMap::from([
                ("br".into(), AssetVariant { hash: hex_hash(0xBB), size: 1500 }),
                ("gzip".into(), AssetVariant { hash: gz_hash.clone(), size: 2500 }),
            ]),
        );
        let chosen = pick_variant(&hit, Some("gzip"));
        assert_eq!(chosen.encoding.as_deref(), Some("gzip"));
        assert_eq!(chosen.hash, gz_hash);
    }

    #[test]
    fn pick_variant_identity_explicit() {
        let hit = static_hit_with_variants(
            0x05,
            1024,
            HashMap::from([(
                "br".into(),
                AssetVariant { hash: hex_hash(0xBB), size: 256 },
            )]),
        );
        let chosen = pick_variant(&hit, Some("identity"));
        assert!(chosen.encoding.is_none(), "identity-only → identity served");
    }

    #[test]
    fn pick_variant_q_zero_rejects() {
        // gzip;q=0 means "do NOT send gzip". Server must fall back to
        // brotli (still accepted) or identity (always available).
        let hit = static_hit_with_variants(
            0x06,
            1024,
            HashMap::from([
                ("br".into(), AssetVariant { hash: hex_hash(0xBB), size: 256 }),
                ("gzip".into(), AssetVariant { hash: hex_hash(0x6F), size: 384 }),
            ]),
        );
        let chosen = pick_variant(&hit, Some("br, gzip;q=0"));
        assert_eq!(chosen.encoding.as_deref(), Some("br"));
    }

    #[test]
    fn pick_variant_unknown_encoding_falls_back_to_identity() {
        let hit = static_hit_with_variants(
            0x07,
            1024,
            HashMap::from([(
                "br".into(),
                AssetVariant { hash: hex_hash(0xBB), size: 256 },
            )]),
        );
        // Client only accepts `lz4` which we don't have a variant for
        // → identity falls through.
        let chosen = pick_variant(&hit, Some("lz4"));
        assert!(chosen.encoding.is_none());
    }

    // ── End-to-end serve path with variants ─────────────────────────────────

    #[compio::test]
    async fn accept_encoding_br_picks_brotli_variant() {
        let (disk, root) = fresh_disk_cache("ae-br");
        let mock = MockHandle::new();
        let identity_hash = hex_hash(0x10);
        let br_hash = hex_hash(0xB1);
        let identity_payload = vec![0xAAu8; 4096];
        let br_payload = vec![0xBBu8; 1024];
        mock.put(&identity_hash, &identity_payload);
        mock.put(&br_hash, &br_payload);

        let state = make_state(mock.store(), disk);
        let hit = static_hit_with_variants(
            0x10,
            identity_payload.len() as u64,
            HashMap::from([(
                "br".into(),
                AssetVariant {
                    hash: br_hash.clone(),
                    size: br_payload.len() as u64,
                },
            )]),
        );

        let req = ntex::web::test::TestRequest::default()
            .header("accept-encoding", "br, gzip")
            .to_http_request();
        let mut resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::OK);
        assert_eq!(hdr(&resp, "content-encoding").as_deref(), Some("br"));
        assert!(
            hdr(&resp, "vary").as_deref().is_some_and(|v| v.contains("Accept-Encoding")),
            "Vary header must mention Accept-Encoding: {:?}",
            hdr(&resp, "vary")
        );
        // ETag is the variant hash, not identity.
        assert_eq!(
            hdr(&resp, "etag").as_deref(),
            Some(format!("\"{}\"", br_hash).as_str())
        );
        // Body is the brotli bytes.
        let got = collect_body(resp.take_body()).await;
        assert_eq!(got, br_payload);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn accept_encoding_identity_picks_no_variant() {
        let (disk, root) = fresh_disk_cache("ae-identity");
        let mock = MockHandle::new();
        let identity_hash = hex_hash(0x11);
        let identity_payload = vec![0xAAu8; 4096];
        mock.put(&identity_hash, &identity_payload);

        let state = make_state(mock.store(), disk);
        let hit = static_hit_with_variants(
            0x11,
            identity_payload.len() as u64,
            HashMap::from([(
                "br".into(),
                AssetVariant { hash: hex_hash(0xB2), size: 100 },
            )]),
        );

        let req = ntex::web::test::TestRequest::default()
            .header("accept-encoding", "identity")
            .to_http_request();
        let mut resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::OK);
        assert!(
            resp.headers().get("content-encoding").is_none(),
            "no Content-Encoding when identity served"
        );
        // Body is the identity bytes.
        let got = collect_body(resp.take_body()).await;
        assert_eq!(got, identity_payload);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn accept_encoding_missing_picks_identity() {
        let (disk, root) = fresh_disk_cache("ae-missing");
        let mock = MockHandle::new();
        let identity_hash = hex_hash(0x12);
        let identity_payload = vec![0xAAu8; 4096];
        mock.put(&identity_hash, &identity_payload);

        let state = make_state(mock.store(), disk);
        let hit = static_hit_with_variants(
            0x12,
            identity_payload.len() as u64,
            HashMap::from([(
                "br".into(),
                AssetVariant { hash: hex_hash(0xB3), size: 100 },
            )]),
        );

        // No Accept-Encoding header → identity.
        let req = bare_request();
        let mut resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::OK);
        assert!(resp.headers().get("content-encoding").is_none());
        let got = collect_body(resp.take_body()).await;
        assert_eq!(got, identity_payload);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn vary_accept_encoding_present_when_variant_chosen() {
        let (disk, root) = fresh_disk_cache("vary-ae");
        let mock = MockHandle::new();
        let br_hash = hex_hash(0xB4);
        let identity_hash = hex_hash(0x13);
        mock.put(&identity_hash, &vec![0xAAu8; 4096]);
        mock.put(&br_hash, &vec![0xBBu8; 1024]);

        let state = make_state(mock.store(), disk);
        let hit = static_hit_with_variants(
            0x13,
            4096,
            HashMap::from([(
                "br".into(),
                AssetVariant { hash: br_hash.clone(), size: 1024 },
            )]),
        );

        let req = ntex::web::test::TestRequest::default()
            .header("accept-encoding", "br")
            .to_http_request();
        let resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        let vary = hdr(&resp, "vary").expect("vary header set when variant chosen");
        assert!(vary.contains("Accept-Encoding"), "Vary contains Accept-Encoding: {vary}");

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn if_none_match_per_variant_etag() {
        // Client previously fetched the brotli variant; on revisit it
        // sends `If-None-Match: "<br_hash>"` and we must 304 — even
        // though the identity hash differs.
        let (disk, root) = fresh_disk_cache("inm-variant");
        let mock = MockHandle::new();
        let br_hash = hex_hash(0xB5);
        let state = make_state(mock.store(), disk);
        let hit = static_hit_with_variants(
            0x14,
            4096,
            HashMap::from([(
                "br".into(),
                AssetVariant { hash: br_hash.clone(), size: 1024 },
            )]),
        );

        let req = ntex::web::test::TestRequest::default()
            .header("accept-encoding", "br")
            .header("if-none-match", format!("\"{}\"", br_hash))
            .to_http_request();
        let resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::NOT_MODIFIED);
        // 304 must carry Vary so caches don't conflate variants.
        let vary = hdr(&resp, "vary").expect("vary on 304 with variant");
        assert!(vary.contains("Accept-Encoding"));
        // Backend was never called — pure ETag short-circuit.
        assert_eq!(mock.calls_for(&br_hash), 0);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn range_uses_variant_size() {
        // Range against a variant uses the variant's compressed size in
        // the Content-Range header total. A client that sees
        // `Content-Encoding: br` and asks for `bytes=0-9` against the
        // 1024-byte brotli body must get back `bytes 0-9/1024`, not
        // `0-9/4096`.
        let (disk, root) = fresh_disk_cache("range-variant");
        let mock = MockHandle::new();
        let br_hash = hex_hash(0xB6);
        let br_size: u64 = 1024;
        mock.put(&br_hash, &vec![0xBBu8; br_size as usize]);

        let state = make_state(mock.store(), disk);
        let hit = static_hit_with_variants(
            0x15,
            4096,
            HashMap::from([(
                "br".into(),
                AssetVariant { hash: br_hash.clone(), size: br_size },
            )]),
        );

        let req = ntex::web::test::TestRequest::default()
            .header("accept-encoding", "br")
            .header("range", "bytes=0-9")
            .to_http_request();
        let resp = serve_static_hit(&state, &req, hit, std::time::Instant::now()).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            hdr(&resp, "content-range").as_deref(),
            Some("bytes 0-9/1024"),
            "range total uses variant size"
        );
        // Variant headers still apply to 206.
        assert_eq!(hdr(&resp, "content-encoding").as_deref(), Some("br"));

        std::fs::remove_dir_all(&root).ok();
    }

    // -----------------------------------------------------------------------
    // Per-rule rate-limit bucket key derivation
    // -----------------------------------------------------------------------

    use zeroship_core::types::RateLimitPer;

    #[test]
    fn compute_bucket_id_app_returns_constant() {
        // RateLimitPer::App always returns "app" regardless of IP or
        // cookie state — every caller shares the same bucket.
        let req = ntex::web::test::TestRequest::default()
            .header("cookie", "__zs_session=abc")
            .to_http_request();
        assert_eq!(compute_bucket_id(&req, RateLimitPer::App), "app");
    }

    #[test]
    fn compute_bucket_id_ip_falls_back_to_unknown() {
        // The TestRequest has no peer addr → "unknown" sentinel keeps
        // the bucket lookup well-defined instead of crashing.
        let req = ntex::web::test::TestRequest::default().to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::Ip);
        assert_eq!(id, "unknown");
    }

    #[test]
    fn compute_bucket_id_session_uses_cookie() {
        let req = ntex::web::test::TestRequest::default()
            .header(
                "cookie",
                "other=foo; __zs_session=abc123; trailing=x",
            )
            .to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::Session);
        assert_eq!(id, "abc123");
    }

    #[test]
    fn compute_bucket_id_session_falls_back_to_ip_when_cookie_missing() {
        // Anonymous caller (no __zs_session) → fall back to IP. The
        // TestRequest has no peer → "unknown".
        let req = ntex::web::test::TestRequest::default()
            .header("cookie", "other=foo")
            .to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::Session);
        assert_eq!(id, "unknown");
    }

    #[test]
    fn extract_session_cookie_handles_empty_value() {
        // `__zs_session=` (empty value) → None, so the caller falls
        // back to IP. Treating empty as a real bucket key would
        // collapse every cookie-empty client into one shared bucket.
        assert_eq!(extract_session_cookie(Some("__zs_session=")), None);
        assert_eq!(extract_session_cookie(None), None);
        assert_eq!(extract_session_cookie(Some("other=foo")), None);
    }

    // -----------------------------------------------------------------------
    // Wiring smoke test — bucket lookup matches what the router does
    // -----------------------------------------------------------------------
    //
    // The Outcome::Worker arm wires `compute_bucket_id` and
    // `state.per_rule_rate_limits.check(...)` together. Driving the full
    // `execute_outcome` would need a constructable `web::types::State`,
    // which ntex doesn't expose outside its `App` builder. Instead we
    // recreate the exact bucket-key composition the router uses and
    // assert it agrees with the registry's view of "drained vs fresh"
    // — same code path in two parts.

    use zeroship_core::types::RateLimit;

    #[test]
    fn router_wiring_bucket_id_matches_registry_key() {
        // rps=1 with RateLimitPer::Ip. First call fills, second 429s.
        // The bucket key is `compute_bucket_id(req, RateLimitPer::Ip)`
        // — verifies the IP-derived discriminator is the same string
        // the registry's key uses, otherwise the second call would
        // hit a fresh bucket and pass.
        let reg = crate::enforce::PerRuleRateLimitRegistry::new();
        let app_id = uuid::Uuid::nil();
        let rl = RateLimit { rps: Some(1), rpm: None, per: RateLimitPer::Ip };
        let req = ntex::web::test::TestRequest::default().to_http_request();
        let bucket_id = compute_bucket_id(&req, rl.per);
        assert!(reg.check(&app_id, 0, rl.per, &bucket_id, &rl).is_ok());
        // Second call with the same request → same bucket id → drained.
        let bucket_id2 = compute_bucket_id(&req, rl.per);
        assert_eq!(bucket_id, bucket_id2, "bucket id is stable for same request");
        let err = reg
            .check(&app_id, 0, rl.per, &bucket_id2, &rl)
            .expect_err("second call must 429 — bucket key matched");
        assert_eq!(err.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn router_wiring_session_buckets_separate_from_ip_buckets() {
        // Two requests carrying distinct __zs_session cookies under
        // RateLimitPer::Session must hit independent buckets even when
        // the IP is the same.
        let reg = crate::enforce::PerRuleRateLimitRegistry::new();
        let app_id = uuid::Uuid::nil();
        let rl = RateLimit { rps: Some(1), rpm: None, per: RateLimitPer::Session };

        let req_a = ntex::web::test::TestRequest::default()
            .header("cookie", "__zs_session=user-a")
            .to_http_request();
        let req_b = ntex::web::test::TestRequest::default()
            .header("cookie", "__zs_session=user-b")
            .to_http_request();
        let bucket_a = compute_bucket_id(&req_a, rl.per);
        let bucket_b = compute_bucket_id(&req_b, rl.per);
        assert_eq!(bucket_a, "user-a");
        assert_eq!(bucket_b, "user-b");
        assert!(reg.check(&app_id, 0, rl.per, &bucket_a, &rl).is_ok());
        assert!(reg.check(&app_id, 0, rl.per, &bucket_b, &rl).is_ok());
        // Reusing user-a within the same second 429s.
        let err = reg
            .check(&app_id, 0, rl.per, &bucket_a, &rl)
            .expect_err("user-a drained");
        assert_eq!(err.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
    }
}

// ---------------------------------------------------------------------------
// Resource-tree request-level tests — exercise the per-resource policy
// gates on synthetic requests built via ntex's `TestRequest`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod resource_tree_tests {
    use super::*;
    use std::collections::HashMap;
    use crate::compiled::{CompiledManifest, EffectivePolicy};
    use zeroship_core::types::{
        AuthLevel, Manifest, ProcedureKind, RateLimit, RateLimitPer, ResourceEntry,
    };

    fn manifest_with_resources(resources: HashMap<String, ResourceEntry>) -> Manifest {
        Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        }
    }

    #[test]
    fn lookup_finds_rpc_resource_after_strip_prefix() {
        let mut resources = HashMap::new();
        resources.insert(
            "rpc:listTodos".into(),
            ResourceEntry {
                kind: Some(ProcedureKind::Query),
                ..Default::default()
            },
        );
        let m = manifest_with_resources(resources);
        let c = CompiledManifest::compile(&m);
        let p = c
            .lookup_resource("/_zs/v1/listTodos")
            .expect("matches rpc:listTodos");
        assert_eq!(p.kind, Some(ProcedureKind::Query));
        // Bare /_rpc/ paths are no longer dispatched — `/_zs/v1/` is
        // the only RPC wire prefix.
        assert!(c.lookup_resource("/_rpc/listTodos").is_none());
    }

    #[test]
    fn auth_satisfied_passes_anon() {
        let policy = EffectivePolicy {
            auth: AuthLevel::Anon,
            rate_limit: None,
            cors: None,
            cache: None,
            csrf_origins: None,
            idempotent: false,
            max_input_bytes: None,
            middleware: vec![],
            publicly_accessible: true,
            kind: Some(ProcedureKind::Query),
            action: crate::compiled::ResolvedAction::WorkerRpc,
            input_schema: None,
            output_schema: None,
        };
        let req = ntex::web::test::TestRequest::default().to_http_request();
        assert!(auth_satisfied(&req, &policy, "secret", &uuid::Uuid::nil()));
    }

    #[test]
    fn auth_satisfied_user_blocks_unauthenticated() {
        let policy = EffectivePolicy {
            auth: AuthLevel::User,
            rate_limit: None,
            cors: None,
            cache: None,
            csrf_origins: None,
            idempotent: false,
            max_input_bytes: None,
            middleware: vec![],
            publicly_accessible: false,
            kind: Some(ProcedureKind::Mutation),
            action: crate::compiled::ResolvedAction::WorkerRpc,
            input_schema: None,
            output_schema: None,
        };
        // No __zs_session cookie → auth fails.
        let req = ntex::web::test::TestRequest::default().to_http_request();
        assert!(!auth_satisfied(&req, &policy, "secret", &uuid::Uuid::nil()));
    }

    #[test]
    fn auth_satisfied_falls_open_when_secret_unset() {
        // Dev / test mode: no auth_secret means the gateway can't verify
        // cookies — pass through and let the worker enforce.
        let policy = EffectivePolicy {
            auth: AuthLevel::User,
            rate_limit: None,
            cors: None,
            cache: None,
            csrf_origins: None,
            idempotent: false,
            max_input_bytes: None,
            middleware: vec![],
            publicly_accessible: false,
            kind: Some(ProcedureKind::Mutation),
            action: crate::compiled::ResolvedAction::WorkerRpc,
            input_schema: None,
            output_schema: None,
        };
        let req = ntex::web::test::TestRequest::default().to_http_request();
        assert!(auth_satisfied(&req, &policy, "", &uuid::Uuid::nil()));
    }

    #[test]
    fn resource_key_hash_is_stable() {
        let a = resource_key_hash("rpc:todos.list");
        let b = resource_key_hash("rpc:todos.list");
        assert_eq!(a, b, "deterministic across calls");
        let c = resource_key_hash("rpc:todos.add");
        assert_ne!(a, c, "different keys hash differently");
    }

    #[test]
    fn rate_limit_resolution_min_wins_for_resource_tree() {
        // End-to-end: build a manifest with a stricter child rate limit
        // and verify the compiled policy reflects min(parent, child).
        let mut resources = HashMap::new();
        resources.insert(
            "*".into(),
            ResourceEntry {
                auth: Some(AuthLevel::User),
                rate_limit: Some(RateLimit { rpm: Some(600), rps: None, per: RateLimitPer::Ip }),
                ..Default::default()
            },
        );
        resources.insert(
            "rpc:expensive".into(),
            ResourceEntry {
                kind: Some(ProcedureKind::Mutation),
                rate_limit: Some(RateLimit { rpm: Some(10), rps: None, per: RateLimitPer::Ip }),
                r#override: vec!["rate_limit".into()],
                ..Default::default()
            },
        );
        let m = manifest_with_resources(resources);
        let c = CompiledManifest::compile(&m);
        let p = c.lookup_resource("/_zs/v1/expensive").expect("matches");
        assert_eq!(
            p.rate_limit.as_ref().unwrap().rpm,
            Some(10),
            "child's stricter cap survives the merge"
        );
    }

    #[test]
    fn passthrough_manifest_has_only_root_default() {
        // The synthesized passthrough manifest carries the `*` root
        // default but no other resources. URL paths fall through to
        // 404 in the gateway; the worker is never invoked.
        let m = Manifest::passthrough();
        let c = CompiledManifest::compile(&m);
        assert!(
            c.lookup_resource("/anything").is_none(),
            "no per-path resource synthesized in passthrough"
        );
        assert!(
            c.lookup_resource("/_zs/v1/anything").is_none(),
            "no RPC resource synthesized in passthrough"
        );
        assert!(
            c.lookup_resource("/_rpc/listTodos").is_none(),
            "legacy /_rpc/ prefix is no longer routed"
        );
    }
}
