# Blob store + edge serving

The storage and serving layer that backs `.zsapp`. Content-addressed bytes, edge-cached on the gateway, served zero-copy where possible.

## Trait

```rust
pub trait BlobStore: Send + Sync + std::fmt::Debug {
    /// Fetch a blob by hash. Allocates.
    async fn get_blob(&self, hash: &str) -> Result<Bytes, BlobError>;

    /// Local on-disk path of a blob, if file-backed. Used by the
    /// gateway for mmap / sendfile zero-copy. Returns None for purely
    /// remote backends.
    fn local_path(&self, hash: &str) -> Option<PathBuf>;

    /// Insert a blob. Idempotent — repeated puts of the same hash
    /// are no-ops by content equivalence.
    async fn put_blob(&self, hash: &str, data: &[u8]) -> Result<(), BlobError>;

    async fn has_blob(&self, hash: &str) -> Result<bool, BlobError>;

    /// Manifest storage — separate keyspace from blobs.
    async fn put_manifest(&self, app_id: &Uuid, deploy_hash: &str, json: &[u8]) -> Result<(), BlobError>;
    async fn get_manifest(&self, app_id: &Uuid, deploy_hash: &str) -> Result<Bytes, BlobError>;
}

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("blob not found: {0}")]
    NotFound(String),
    #[error("hash mismatch: expected {expected}, got {got}")]
    HashMismatch { expected: String, got: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend: {0}")]
    Backend(String),
}
```

The `local_path` accessor is the zero-copy hook. Implementations expose it iff the blob is on a local filesystem the caller can `mmap` or `sendfile` from.

## Multi-tenancy and possession proof

The trait operates on a **single global keyspace** (`blobs/<hash>`). Cross-tenant dedup is safe by construction under two protocol constraints (see `docs/reference/zsapp.md` for the full argument):

1. Clients always upload every blob in their deploy — no client-side `has_blob` skip.
2. The server verifies `sha256(bytes) == hash` on every `put_blob`.

Together these give a cryptographic possession proof: any successful deploy referencing hash `X` proves the deployer had the bytes for `X`. Cross-tenant dedup therefore can't leak content — anyone with access to hash `X` already had the bytes via their own manifest.

Consequences for this layer:

- `has_blob` is **internal-only**. It MUST NOT be exposed over any HTTP endpoint — doing so would create an oracle that defeats the possession-proof argument.
- The gateway MUST NOT serve a route shaped like `GET /blobs/<hash>`. Bytes are served only via manifest-routed paths (e.g., `assets["/foo.js"].hash` → fetch).
- No per-tenant scoping in `BlobStore` method signatures. `get_blob(hash)`, `put_blob(hash, data)` are scope-free.
- Server-internal dedup is implemented inside `put_blob` (idempotent on identical hash+content already-present).

## Implementations

### `LocalDiskBlobStore`

```
<root>/blobs/<hash[0..2]>/<hash[2..]>            ← sharded by 2-char prefix
<root>/manifests/<app_id>/<deploy_hash>.json
```

`local_path` returns `Some(path)`. Used in dev / single-host prod (gateway and control share a volume).

### `S3BlobStore` (later phase)

Talks to S3/R2 via `cyper` (already in workspace deps). Keys map directly:

```
s3://<bucket>/blobs/<hash>
s3://<bucket>/manifests/<app_id>/<deploy_hash>.json
```

`local_path` returns `None`.

### `CachedBlobStore<Backend>` (the edge wrapper)

Wraps any backend with two cache tiers:

```
                   ┌── mem LRU (Bytes-keyed, ~256 MB default)
       get_blob ───┤
                   └── disk LRU (file-backed, ~20 GB default)
                       │
                       ▼ miss
                   backend.get_blob()
```

On miss: fetch from backend, write to disk LRU (so subsequent serves can mmap), insert into mem LRU. Return `Bytes`.

`local_path` returns `Some(disk_cache_path)` once the blob is in the disk LRU — even when the backing store is remote (S3). This is what makes mmap+sendfile work behind a cloud-storage origin.

Cache coherence is **free**: blob hashes are content-stable, so cache entries never go stale. Invalidation is purely about footprint (LRU eviction).

## Storage layout (recap)

Whether `LocalDiskBlobStore` or `S3BlobStore`:

```
blobs/<hash>                              ← shared across apps and deploys
manifests/<app_id>/<deploy_hash>.json    ← immutable per deploy
```

Active deploy is a column on the `apps` table (`deploy_hash` + `manifest_json`). Rollback is a single UPDATE.

## Edge cache (in `crates/gateway`)

```
GateState {
    blob_store: Arc<dyn BlobStore>,        ← typically CachedBlobStore<S3BlobStore>
    routes: RouteCache,
    ...
}
```

Configuration:

```bash
zeroship-gate \
  --blob-store local:/var/zeroship/blobs              # dev / shared volume
  # OR
  --blob-store s3://prod-bucket?region=us-east-1      # multi-region prod
  --blob-cache /var/cache/zeroship/blobs              # disk LRU root (CachedBlobStore)
  --blob-cache-mem-mb 256
  --blob-cache-disk-gb 20
```

The control plane is **not on the hot path** for static asset bytes. It's consulted only for manifest sync (every 5s, small JSON).

## Zero-copy serving — three phases

### Phase A (Phase 4 of the rollout)

Memory LRU in front of the underlying `BlobStore`. Serve as `Bytes`:

```rust
async fn serve_static_hit(state, hit) -> HttpResponse {
    let bytes = match state.blob_cache.get(&hit.hash) {
        Some(b) => b,
        None => {
            let b = state.blob_store.get_blob(&hit.hash).await?;
            state.blob_cache.insert(hit.hash.clone(), b.clone());
            b
        }
    };
    HttpResponse::Ok()
        .content_type(&hit.content_type)
        .header("etag", format!("\"{}\"", hit.hash))
        .header("cache-control", cache_control_header(&hit.cache))
        .body(bytes)              // ntex writes Bytes to socket — one userspace→kernel copy
}
```

`Bytes` is `Arc`-refcounted; concurrent requests for the same hash share the buffer. The cache (`BlobCache` in `crates/gateway/src/blob_cache.rs`) is bounded by total bytes (default 256 MB, overridable with `--blob-cache-mem-mb`); a single entry larger than the budget is rejected so one fat asset can't evict everything else.

This eliminates the network round-trip to the control plane — by far the dominant cost. Ships in Phase 4.

### Phase B (Phase 5 of the rollout)

Disk LRU + mmap. When the blob is on local disk:

```rust
if let Some(path) = state.blob_store.local_path(&hit.hash) {
    let mmap = unsafe { memmap2::Mmap::map(&File::open(&path)?)? };
    let bytes = Bytes::from_owner(mmap);  // zero-copy wrap; mmap holds the bytes
    return HttpResponse::Ok()
        .content_type(&hit.content_type)
        .header("etag", format!("\"{}\"", hit.hash))
        .header("cache-control", cache_control_header(&hit.cache))
        .body(bytes);
}
// fallback: cold path → backend → fill → mmap
```

The kernel handles page-cache hits transparently. The body write becomes a vectorized send straight from the page cache to the socket.

`unsafe`: blobs are content-addressed and immutable once written; the unsafety is bounded. The workspace lint denies `unsafe_code`; opt-out at the module level (`#![allow(unsafe_code)]` with a `// SAFETY:` justification).

### Phase C (Phase 6 of the rollout)

Two parts: a streaming-body path that lands today, and a true sendfile path that remains future work.

**What ships now: streaming bodies for large blobs.**

`crates/gateway/src/router.rs::serve_static_hit` dispatches on `hit.size`:

- Below `STREAM_THRESHOLD_BYTES` (1 MiB, configurable via the constant): the Phase A/B buffered path — mem LRU → disk LRU (mmap) → backend, full body written in one go. `Bytes` is `Arc`-refcounted so concurrent requests for the same hash share the buffer.
- At or above the threshold: the streaming path. `compio::fs::File::read_at` reads the on-disk LRU file in 64 KiB chunks (`STREAM_CHUNK_BYTES`) and feeds them into `ntex::http::body::SizedStream`, which preserves Content-Length and serves identity transfer encoding. The mem LRU is **intentionally skipped** for streamed serves — a multi-MB blob would either bypass the per-entry budget cap or silently fail to cache, neither of which helps. The disk LRU is still filled on a cold backend miss so subsequent serves hit the warm path.

This eliminates the multi-MB user-space allocation that Phase B still required (mmap'd `Bytes` are cheap, but `Bytes::copy_from_slice` over to `ntex_bytes::Bytes` for the response body still cost a full-size allocation per request). Streaming starts immediately, RSS doesn't balloon for fat assets, and slow-client backpressure is honoured per-chunk.

The chunk-by-chunk write still goes through user space — `ntex_bytes::Bytes::from(Vec<u8>)` copies — so this is **not** true zero-copy. The win is bounded RSS and time-to-first-byte.

**What's still future work: true zero-copy via `sendfile(2)` or `IORING_OP_SPLICE`.**

The remaining page-cache → socket copy goes away if the gateway can hand a file descriptor + offset/length to the kernel. Catch: ntex's `HttpResponse` doesn't expose the underlying socket fd. Two options:

- Custom response path for static rules — short-circuit before ntex's response builder, take ownership of the socket, splice file → socket.
- Patch ntex to accept a "serve from fd" body type.

Both reach past ntex's abstractions and require either a custom fork or upstreaming. Only worth doing if profiling shows the per-chunk user-space copy is the bottleneck — for typical asset-serving workloads, TCP/TLS overhead dominates. The streaming path above gives most of the benefit with none of the abstraction violation.

## Garbage collection

Background sweep, runs weekly (configurable):

```
1. Read all rows: SELECT id, deploy_hash, manifest_json FROM apps
2. Optionally walk manifests/<app_id>/*.json to retain N most recent deploys per app
3. Build set R = union of all hashes referenced
4. For each blob in blobs/, if mtime > retention_days AND hash ∉ R: delete
```

Tunables:
- `retention_days` (default 30) — protects blobs whose manifest just landed but isn't yet in cache state.
- `keep_n_deploys_per_app` (default 5) — how many historical deploys per app to keep manifests + their blobs alive for. Supports rollback.

## Failure modes

| Cause | Behavior |
| --- | --- |
| Mem LRU miss + disk LRU hit | mmap from disk; promote to mem |
| Disk LRU miss | fetch backend; fill both caches |
| Backend (S3) unavailable | 502 to the asset request; do not poison caches with the error |
| Disk full on cache fill | skip cache fill; serve from in-memory; log warning |
| Hash mismatch on read (data corruption) | invalidate cache entry, re-fetch from backend, log error |
| `local_path` returns Some but the file is missing | re-fetch via `get_blob`, refill cache |

## Callers

- `crates/control/src/main.rs` — boots `Arc<dyn BlobStore>` (`LocalDiskBlobStore` in dev).
- `crates/control/src/deploy.rs` — `.zsapp` ingestion. Verifies hashes, deduplicates against `has_blob`, calls `put_blob` for new content, writes the manifest via `put_manifest`.
- `crates/gateway/src/router.rs::serve_static_hit` — fetches asset bytes via `state.blob_cache.get(hash)` then falls through to `state.blob_store.get_blob(hash)`.
- `crates/worker/src/sync.rs` — fetches `manifest.worker.modules[entry]` via `BlobStore` on cold start / deploy change.

## What this architecture intentionally does NOT do

- **No CDN integration.** Edge nodes ARE the CDN. CloudFront/Fastly can be layered later if multi-continent traffic demands it.
- **No automatic blob compression negotiation.** The build pipeline pre-compresses assets if it wants to (Brotli, gzip variants) and stores them as separate blobs with the right `content_type`. Selecting variants by `Accept-Encoding` is gateway logic, not blob-store logic.
- **No partial range serving.** Range request support belongs to a higher HTTP layer; the blob store doesn't need to know about it.
- **No streaming write API.** `put_blob` takes `&[u8]`. A streaming `put_blob_stream(hash, impl AsyncRead)` would be added when ingestion outgrows the buffered model.
