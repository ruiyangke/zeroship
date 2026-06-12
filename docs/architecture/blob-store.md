# Blob store + edge serving

The `.zship` deploy path is content-addressed. `crates/bundle/src/blob.rs` defines the storage trait; gateway, control, and worker all read from the same blob root.

## Trait surface

Current `BlobStore` methods:

- `get_blob(hash) -> Result<Bytes, BlobError>`
- `local_path(hash) -> Option<PathBuf>` — `None` for remote backends
- `put_blob(hash, data) -> Result<PutOutcome, BlobError>`
- `put_blob_stream(hash, expected_size, reader) -> Result<PutOutcome, BlobError>`
- `has_blob(hash) -> Result<bool, BlobError>`
- `get_blob_to_file(hash, out, expected_size, max_bytes) -> Result<u64, BlobError>` — streams a blob into an already-open temp file while byte-verifying SHA-256; the gateway's hot-path refill primitive (no whole-object buffering)
- `put_manifest(app_id, deploy_hash, json)`
- `get_manifest(app_id, deploy_hash)`
- `delete_app_manifests(app_id)` — deletes the app's `manifests/<app_id>/` keyspace (shared `blobs/` are untouched); used by control's `purge_app`

`PutOutcome` is part of the current contract. Ingest uses it to count fresh writes vs dedupe hits without a separate preflight call. There are no default trait methods: every backend implements every method.

## Implementations

Two shipping backends, selected per process by `--blob-store` / `BLOB_STORE`:

- a bare path (the dev default, e.g. `./bundles`) → `LocalDiskBlobStore`
- an `s3://bucket/prefix?region=…` URL → `S3BlobStore` (over `compio-s3`)

`LocalDiskBlobStore` layout:

```text
<root>/blobs/<hash[0..2]>/<hash[2..]>
<root>/manifests/<app_id>/<deploy_hash>.json
```

`S3BlobStore` keyspace (under the config prefix):

```text
{prefix}/blobs/<sha256>
{prefix}/manifests/<app_id>/<deploy_hash>.json
```

Important behavior:

- Blob keys are global by hash. There is no per-app blob namespace, so identical bytes (a shared dependency, an unchanged asset across deploys) are stored once and deduped across every app.
- `put_blob_stream` re-hashes bytes while reading and rejects size/hash mismatches. On `S3BlobStore` it streams the single-pass reader through **multipart** in `PART_SIZE` (8 MiB) chunks while running a whole-object SHA-256 hasher, verifies `sha256(stream) == hash` BEFORE `complete_multipart` (aborting on mismatch so nothing is ever committed under the wrong key), and uses a single `PutObject` for objects below one part. This preserves the same content-addressing integrity guarantee `LocalDiskBlobStore` gives, across parts.
- `LocalDiskBlobStore::local_path` is a cheap path computation; it does not check whether the file exists. `S3BlobStore::local_path` is always `None` — the gateway hot path uses the disk-cache refill below, not `local_path`.
- Reads validate the stored bytes again (re-hash) and return `BlobError::HashMismatch` if content is corrupt. `get_blob_to_file` re-verifies SHA-256 on the way out before the caller publishes.
- Manifest writes are immutable-key: the key embeds `deploy_hash`, so `put_manifest` uses a conditional create (`If-None-Match: *`); an identical replay is success, divergent content is a backend error.

## Current ingest path

`zeroship_bundle::ingest` is the `.zship` ingestion path used by control:

```text
manifest.json first
  -> validate manifest
  -> compute deploy_hash
  -> stream each blobs/<hash> entry through `put_blob_stream`
  -> write manifests/<app_id>/<deploy_hash>.json
```

The upload must contain every referenced blob. The server re-checks every hash; dedupe is an internal store concern, not a client-side `has_blob` round trip.

## Gateway-side caching

Gateway adds two caches on top of `BlobStore`:

- `BlobCache` in [blob_cache.rs](../../crates/gateway/src/blob_cache.rs): in-memory LRU
- `DiskBlobCache` in the same file: on-disk LRU used for mmap and streaming

Static serving lives in [static_serve.rs](../../crates/gateway/src/router/static_serve.rs):

- small responses: memory LRU -> disk LRU/mmap -> blob store (`get_blob`)
- large responses: ensure a disk copy exists, then stream in chunks from disk
- conditional requests, range requests, and pre-compressed variants are handled at this HTTP layer, not in `BlobStore`

On a disk-cache miss the gateway does NOT use `BlobStore::local_path` (a remote store returns `None`). Instead it **streams** the blob from the store into the disk cache without buffering the whole object:

1. `DiskBlobCache::reserve_temp(hash)` opens a unique temp file (`create_new`) under the cache root and returns a `DiskBlobTemp` guard that unlinks on drop unless published.
2. `BlobStore::get_blob_to_file(hash, temp.file(), expected_size, MAX_BLOB_BYTES)` streams + byte-verifies into that open file (the store/S3 client never receive a raw path).
3. `DiskBlobCache::publish_temp` `sync_all`s, then publishes under the content-addressed final path with a **no-clobber** primitive (hard-link → unlink temp; copy into a `create_new` final if hard-link is unsupported; never overwrite-rename) and verifies any pre-existing final file before trusting it.

A per-process **singleflight** keyed by blob hash collapses concurrent cold misses; duplicate downloads remain safe because correctness comes from temp uniqueness + no-clobber publish + final-file verification (multi-process safe over a shared cache root).

The control plane is not on the hot path for asset bytes.

## Current callers

All three services build their store from the SAME `--blob-store` grammar via `zeroship_bundle::build_blob_store` (`StoreUrl::parse` → `LocalDiskBlobStore` or `S3BlobStore`), so control's deploy ingest writes through exactly the store gateway and worker read.

- [crates/control/src/main.rs](../../crates/control/src/main.rs): builds the store; the deploy ingest writes blobs + manifests through it. There is no separate per-app `BundleStore`/VFS — `purge_app` deletes the app's manifest keyspace via `delete_app_manifests`.
- [crates/gateway/src/main.rs](../../crates/gateway/src/main.rs): builds the store plus memory/disk caches; refills the disk cache by streaming `get_blob_to_file`.
- [crates/worker/src/main.rs](../../crates/worker/src/main.rs): builds the store.
- [crates/worker/src/sync.rs](../../crates/worker/src/sync.rs): fetches `manifest.worker.modules[entry]`

`s3://` credentials resolve from the standard AWS environment (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / optional `AWS_SESSION_TOKEN`); there is no provider chain.

## The other S3 consumer: `env.storage`

The same `compio-s3` client and the same `s3://…` URL grammar back the
creator-facing `env.storage` namespace (`crates/plugin-storage`). It is a
**separate** keyspace and a **separate** flag — `--storage-url` on the worker
(and `ZEROSHIP_STORAGE_URL` for `zeroship serve`) — but resolves credentials
from the same AWS env vars (one S3 identity per process). A worker can point
`--blob-store` and `--storage-url` at different prefixes (or different
buckets) of the same provider.

Unlike deploy blobs, `env.storage` objects are **mutable, app-authored, and
unbounded** in size: uploads use S3 **multipart** (8 MiB parts) so a creator
can stream an arbitrarily large object up (`putStream`) and back
(`getStream`) with memory bounded by the part size, never the object size.
See [plugin-system.md](../reference/plugin-system.md) for the streaming
native surface and [docker-compose.md](../runbooks/docker-compose.md) for the
MinIO/R2 configuration.

End-to-end proof that both consumers work over one remote store lives in
`tests/e2e_s3_storage.sh` (MinIO): control writes deploy blobs to S3, the
gateway dispatches a request to the worker that loads the bundle **from S3**,
and a > part-size multipart `env.storage` round-trip is byte-compared.

## Non-goals

- No blob-serving API shaped like `GET /blobs/<hash>`
- No blob-store-specific routing logic; URL/path/variant selection stays in the gateway
- No shared-blob GC/refcounting: content-addressed blobs under `blobs/` are not app-owned and are not deleted on app purge

## Related docs

- [Architecture overview](../architecture/overview.md) — the entry point and system map.
- [Gateway routing](../architecture/gateway-routing.md) — the static-serving consumer of `BlobStore`.
- [Control plane](../architecture/control-plane.md) — runs the `.zship` ingest that writes blobs.
- [Distributed architecture](../architecture/distributed.md) — how the blob root is shared across processes.
- [`.zship` artifact format](../reference/zship.md) — the deploy archive whose blobs land here.
