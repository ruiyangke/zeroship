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
    let (tx, rx) = ntex::channel::mpsc::channel();
    compio::runtime::spawn(async move {
        let file = match compio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(Err::<Bytes, Rc<dyn std::error::Error>>(Rc::new(e)));
                return;
            }
        };
        chunk_stream_from_file(&file, size, chunk_bytes, &tx).await;
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
async fn chunk_stream_from_file<R>(
    source: &R,
    size: u64,
    chunk_bytes: usize,
    tx: &ntex::channel::mpsc::Sender<Result<Bytes, Rc<dyn std::error::Error>>>,
) where
    R: compio::io::AsyncReadAt,
{
    use compio::buf::BufResult;
    let mut offset: u64 = 0;
    while offset < size {
        let want = std::cmp::min(chunk_bytes as u64, size - offset) as usize;
        let buf = vec![0u8; want];
        let BufResult(res, returned) = source.read_at(buf, offset).await;
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
                offset += n as u64;
            }
            Err(e) => {
                let _ = tx.send(Err::<Bytes, Rc<dyn std::error::Error>>(Rc::new(e)));
                return;
            }
        }
    }
}

/// Build a streaming `HttpResponse` for a single static hit whose
/// bytes live on the gateway's disk LRU. Falls back to a buffered
/// response when the file isn't (or can't be) on disk.
async fn serve_static_streaming(
    state: &GateState,
    hit: &crate::dispatch::StaticHit,
    wall_start: std::time::Instant,
) -> HttpResponse {
    let path = match ensure_disk_path(&state.disk_cache, &*state.blob_store, &hit.hash).await {
        DiskAvailability::OnDisk(p) => p,
        DiskAvailability::InMemoryOnly(b) => {
            // Disk fill failed — fall back to buffered. Skip the
            // mem cache: a multi-MB blob would either evict
            // everything else or silently fail the budget check.
            return build_buffered_response(hit, &b, wall_start);
        }
        DiskAvailability::NotFound => {
            return HttpResponse::NotFound()
                .json(&serde_json::json!({"error": "asset bytes missing"}));
        }
        DiskAvailability::Unavailable(err) => {
            eprintln!("[gate] blob fetch error for {}: {err}", hit.hash);
            return HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": "blob store unavailable"}));
        }
    };
    let rx = chunk_stream_from_path(path, hit.size, STREAM_CHUNK_BYTES);
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
    // SizedStream sets Content-Length and uses identity transfer
    // encoding — better for browsers and intermediaries than the
    // chunked encoding `streaming()` would produce.
    resp.body(SizedStream::new(hit.size, rx))
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
async fn serve_static_hit(
    state: &GateState,
    hit: crate::dispatch::StaticHit,
    wall_start: std::time::Instant,
) -> HttpResponse {
    if hit.size >= STREAM_THRESHOLD_BYTES {
        return serve_static_streaming(state, &hit, wall_start).await;
    }
    let bytes = match fetch_static_bytes(
        &state.blob_cache,
        &state.disk_cache,
        &*state.blob_store,
        &hit.hash,
    )
    .await
    {
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
    build_buffered_response(&hit, &bytes, wall_start)
}

/// Build a buffered (single-write) static response. Used by the small
/// branch of `serve_static_hit` and by the streaming path's fallback
/// when a disk insert fails.
fn build_buffered_response(
    hit: &crate::dispatch::StaticHit,
    bytes: &bytes::Bytes,
    wall_start: std::time::Instant,
) -> HttpResponse {
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
    // bytes is either an `Arc<Vec<u8>>` (memory tier) or backed by an
    // mmap (disk tier via `Bytes::from_owner`). Either way ntex needs
    // its own `ntex_bytes::Bytes`; the conversion is one userspace
    // copy today, replaced by `sendfile(2)` in Phase C for streamable
    // sizes (large blobs already take the streaming path).
    resp.body(Bytes::copy_from_slice(bytes))
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
        }
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
        let mut resp = serve_static_hit(&state, hit, std::time::Instant::now()).await;

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
        let mut resp = serve_static_hit(&state, hit, std::time::Instant::now()).await;

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
        let mut resp = serve_static_hit(&state, hit, std::time::Instant::now()).await;

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
        let resp = serve_static_hit(&state, hit, std::time::Instant::now()).await;
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
}
