# Blob store + edge serving

The `.zship` deploy path is content-addressed. `crates/bundle/src/blob.rs` defines the storage trait; gateway, control, and worker all read from the same blob root.

## Trait surface

Current `BlobStore` methods:

- `get_blob(hash) -> Result<Bytes, BlobError>`
- `local_path(hash) -> Option<PathBuf>`
- `put_blob(hash, data) -> Result<PutOutcome, BlobError>`
- `put_blob_stream(hash, expected_size, reader) -> Result<PutOutcome, BlobError>`
- `has_blob(hash) -> Result<bool, BlobError>`
- `put_manifest(app_id, deploy_hash, json)`
- `get_manifest(app_id, deploy_hash)`

`PutOutcome` is part of the current contract. Ingest uses it to count fresh writes vs dedupe hits without a separate preflight call.

## Current implementation

The shipping backend in this worktree is `LocalDiskBlobStore`.

```text
<root>/blobs/<hash[0..2]>/<hash[2..]>
<root>/manifests/<app_id>/<deploy_hash>.json
```

Important current behavior:

- Blob keys are global by hash. There is no per-app blob namespace.
- `put_blob_stream` re-hashes bytes while writing and rejects size/hash mismatches.
- `LocalDiskBlobStore::local_path` is a cheap path computation; it does not check whether the file exists.
- Reads validate the stored bytes again and return `BlobError::HashMismatch` if on-disk content is corrupt.

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

- `BlobCache` in [blob_cache.rs](crates/gateway/src/blob_cache.rs): in-memory LRU
- `DiskBlobCache` in the same file: on-disk LRU used for mmap and streaming

Static serving lives in [static_serve.rs](crates/gateway/src/router/static_serve.rs):

- small responses: memory LRU -> disk LRU/mmap -> blob store
- large responses: ensure disk copy exists, then stream in chunks from disk
- conditional requests, range requests, and pre-compressed variants are handled at this HTTP layer, not in `BlobStore`

The control plane is not on the hot path for asset bytes.

## Current callers

- [crates/control/src/main.rs](crates/control/src/main.rs): constructs `LocalDiskBlobStore`
- [crates/gateway/src/main.rs](crates/gateway/src/main.rs): constructs `LocalDiskBlobStore` plus memory/disk caches
- [crates/worker/src/main.rs](crates/worker/src/main.rs): constructs `LocalDiskBlobStore`
- [crates/worker/src/sync.rs](crates/worker/src/sync.rs): fetches `manifest.worker.modules[entry]`

## Current non-goals

- No blob-serving API shaped like `GET /blobs/<hash>`
- No shipping S3-backed `BlobStore` in this worktree
- No blob-store-specific routing logic; URL/path/variant selection stays in the gateway
