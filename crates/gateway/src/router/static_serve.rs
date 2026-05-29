//! Static-asset serving — resolve a `try` chain against the manifest's
//! asset maps, fetch bytes through the gateway's three-tier cache,
//! handle conditional / range / variant headers, then ship.
//!
//! Two body paths share most of the surface:
//!
//! * Buffered (small blobs): mem LRU → disk LRU (mmap) → backend, then
//!   write the full body in one go.
//! * Streaming (≥ `STREAM_THRESHOLD_BYTES`): warm the disk LRU, read
//!   in 64 KiB chunks via `compio::fs::File::read_at`, feed `SizedStream`.

use std::path::PathBuf;

use ntex::http::body::SizedStream;
use ntex::util::Bytes;
use ntex::web::HttpRequest;
use ntex::web::HttpResponse;

use crate::GateState;

use super::conditional::{etag_matches, parse_range, RangeSpec};
use super::helpers::header_str_borrowed;
use super::streaming::{
    chunk_stream_from_path, chunk_stream_from_path_range, STREAM_CHUNK_BYTES,
    STREAM_THRESHOLD_BYTES,
};
use super::variants::{pick_variant, ChosenVariant};

/// Resolve a static action's `try` chain against the manifest's asset
/// maps. First entry that hits wins; returns 404 on full miss.
pub(super) async fn serve_resource_tree_static(
    state: &GateState,
    compiled_route: &crate::sync::CompiledRoute,
    req: &HttpRequest,
    request_path: &str,
    try_chain: &[String],
    wall_start: std::time::Instant,
) -> HttpResponse {
    // Support the literal templates the build emits, plus the bare
    // `$path` token (used by `/_assets/*` SPA fallback). Captures are
    // still deferred; the build emits literal paths for now.
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
pub(super) fn lookup_static_hit(
    compiled_route: &crate::sync::CompiledRoute,
    path: &str,
) -> Option<crate::dispatch::StaticHit> {
    use zeroship_bundle::CacheCtl;
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

/// Outcome of the cache → blob-store fetch for a static hit. Pulled out
/// so it can be unit-tested with a `MockBlobStore` without constructing a
/// full `GateState`.
pub(super) enum BlobFetch {
    Hit(bytes::Bytes),
    NotFound,
    Unavailable(String),
}

/// Resolve a blob through the gateway's three-tier cache:
///
/// 1. Memory LRU — `BlobCache::get` returns refcounted `Bytes`.
/// 2. Disk LRU — `DiskBlobCache::local_path` returns a path; we
///    `mmap` it and wrap as `Bytes::from_owner(mmap)` for zero-copy
///    serving.
/// 3. Backend — fetch from `BlobStore`, fill both tiers.
///
/// On a disk miss + backend hit we ALWAYS write to the disk cache so
/// subsequent serves take the mmap path. The mem cache is also filled
/// so hot blobs short-circuit before disk I/O.
pub(super) async fn fetch_static_bytes(
    mem: &crate::blob_cache::BlobCache,
    disk: &crate::blob_cache::DiskBlobCache,
    store: &dyn zeroship_bundle::BlobStore,
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
                tracing::warn!(hash = %hash, error = %e, "gateway: mmap failed");
            }
        }
    }
    // Tier 3: backend.
    match store.get_blob(hash).await {
        Ok(b) => {
            // Best-effort disk fill: a failure here doesn't stop the
            // serve. The mem tier still gets the bytes.
            if let Err(e) = disk.insert(hash, &b) {
                tracing::warn!(hash = %hash, error = %e, "gateway: disk cache insert failed");
            }
            mem.insert(hash.to_string(), b.clone());
            BlobFetch::Hit(b)
        }
        Err(zeroship_bundle::BlobError::NotFound(_)) => BlobFetch::NotFound,
        Err(e) => BlobFetch::Unavailable(e.to_string()),
    }
}

/// Outcome of `ensure_disk_path` — the streaming path needs the file
/// available locally, but on a backend-fetch fallback we may already
/// have the bytes in hand and a disk insert may have failed. Callers
/// fall back to a buffered response in that case.
pub(super) enum DiskAvailability {
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
pub(super) async fn ensure_disk_path(
    disk: &crate::blob_cache::DiskBlobCache,
    store: &dyn zeroship_bundle::BlobStore,
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
                tracing::warn!(hash = %hash, error = %e, "gateway: disk cache insert failed");
                DiskAvailability::InMemoryOnly(b)
            }
        },
        Err(zeroship_bundle::BlobError::NotFound(_)) => DiskAvailability::NotFound,
        Err(e) => DiskAvailability::Unavailable(e.to_string()),
    }
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
            tracing::error!(hash = %chosen.hash, error = %err, "gateway: blob fetch error");
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
pub(super) async fn serve_static_hit(
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
            tracing::error!(hash = %chosen.hash, error = %err, "gateway: blob fetch error");
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
    // copy today. Large blobs already take the streaming path, and a
    // future sendfile-style path can remove this copy for the buffered
    // serve path too.
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
pub(super) fn cache_control_header(c: &zeroship_bundle::CacheCtl) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use ntex::web::HttpRequest;

    use crate::blob_cache::{BlobCache, DiskBlobCache};
    use crate::GateState;
    use zeroship_bundle::{BlobError, BlobStore, PutOutcome};

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
        async fn put_blob(&self, hash: &str, data: &[u8]) -> Result<PutOutcome, BlobError> {
            self.put(hash, data);
            Ok(PutOutcome::Wrote)
        }
        async fn put_blob_stream(
            &self,
            hash: &str,
            _expected_size: u64,
            reader: &mut dyn std::io::Read,
        ) -> Result<PutOutcome, BlobError> {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(reader, &mut buf)
                .map_err(|e| BlobError::Backend(e.to_string()))?;
            self.put(hash, &buf);
            Ok(PutOutcome::Wrote)
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
    // Streaming-body tests
    // -----------------------------------------------------------------------

    use ntex::http::body::{Body, BodySize, MessageBody, ResponseBody};

    fn static_hit(hash: &str, size: u64) -> crate::dispatch::StaticHit {
        crate::dispatch::StaticHit {
            path: "/big.bin".into(),
            hash: hash.into(),
            content_type: "application/octet-stream".into(),
            size,
            cache: zeroship_bundle::CacheCtl {
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
        store: Arc<dyn zeroship_bundle::BlobStore>,
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
                hydra_public: String::new(),
                auth_public: String::new(),
                insecure_dev: true,
                trust_proxy: false,
                public_url: "https://api.zeroship.ai".into(),
            },
            routes: crate::sync::RouteCache::new(),
            hash_ring: crate::proxy::HashRing::new(vec!["http://0.0.0.0:0".into()], 1),
            rate_limiters: crate::enforce::RateLimitRegistry::new(1, 1),
            per_rule_rate_limits: crate::enforce::PerRuleRateLimitRegistry::new(),
            concurrency: crate::enforce::ConcurrencyRegistry::new(1),
            blob_store: store,
            blob_cache: BlobCache::new(8 * 1024 * 1024),
            disk_cache: disk,
            idempotency_store: Arc::new(crate::idempotency::InMemoryIdempotencyStore::new()),
            oidc_rp: Arc::new(crate::oidc_rp::OidcRp::new(
                "http://auth.test",
                "gateway",
                "test-secret",
                b"test-stash-key-32-bytes-long----".to_vec(),
            )),
            db: None,
            dpop_jti_cache: Arc::new(zeroship_core::dpop::TieredJtiCache::default()),
            logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
            signing_key: None,
            wrapper_issuer: None,
            wrapper_verifier: None,
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
        fn store(&self) -> Arc<dyn zeroship_bundle::BlobStore> {
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
    fn hdr(resp: &ntex::web::HttpResponse, name: &str) -> Option<String> {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
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

    // ── cache_control_header ────────────────────────────────────────────────

    #[test]
    fn cache_ctl_emits_stale_if_error() {
        // stale_on_error AND swr_window set → both stale-while-revalidate
        // and stale-if-error directives.
        let c = zeroship_bundle::CacheCtl {
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
        let c = zeroship_bundle::CacheCtl {
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
        let c = zeroship_bundle::CacheCtl {
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
    // Tier 4b — Accept-Encoding negotiation: end-to-end serve path
    // -----------------------------------------------------------------------
    //
    // Pure unit tests for `pick_variant` live in `variants.rs::tests`;
    // these cover the full `serve_static_hit` flow with variant-bearing
    // assets so we exercise the variant headers, ETags, ranges, etc.

    use zeroship_bundle::AssetVariant;

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
}
