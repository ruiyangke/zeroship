# Proposal: Production object storage (S3/R2) - ISS-32

**Status:** SHIPPED (branch `design/s3-object-storage`, PR1–PR4) - **Tier:** T1 launch-blocking - **Date:** 2026-06-12

> **Implemented.** This proposal has shipped, including the appended
> "v1 Streaming & Multipart — FULL SCOPE" section (streaming both directions +
> multipart, nothing deferred). The code lives in `crates/compio-s3`
> (zero-tokio cyper + hand-rolled SigV4 client), `crates/bundle`
> (`S3BlobStore`), `crates/plugin-storage` (`S3` backend + V8 streaming), and
> the gateway/control/worker `--blob-store`/`--storage-url` wiring. Verified
> end-to-end against MinIO by `tests/e2e_s3_storage.sh` (deploy-blob dispatch
> from S3 + a > part-size multipart `env.storage` streaming round-trip) and the
> LocalFs-vs-S3 backend-parity suite. One runtime fix landed alongside: the
> upload-streaming `response_forwarder` gained pause/resume backpressure so a
> larger-than-buffer-cap streaming upload no longer overflows. See ISSUES.md
> ISS-32 (FIXED) for the per-PR summary. The sections below are the original
> design and are retained for provenance.

## Problem

`crates/bundle` ships only `LocalDiskBlobStore` and
`crates/plugin-storage` ships only `LocalFs`. Every deploy artifact
(`.zship` blobs + manifests) and every `env.storage` object currently lives on
one node's disk:

- **No durability/replication.** A disk loss is unrecoverable.
- **Multi-worker is not production-correct.** Workers and gateways assume
  deploy blobs are shared, and `env.storage` reads/writes assume the same.
  Today that only works with a shared filesystem.

**Goal:** one production object-storage backend usable by both abstractions:

- `zeroship-bundle::BlobStore` for `.zship` content-addressed blobs and
  manifests.
- `zeroship-plugin-storage::Backend` for creator-facing `env.storage`.

Control, gateway, and worker must all be able to use the same remote deploy
blob store in the launch slice. Control is the writer of deploy blobs via
`deploy::ingest(&state.blob_store, ...)`; if control stayed local while
gateway/worker read S3, deploys could not populate the store they serve from.

Targets: AWS S3, Cloudflare R2, MinIO, Backblaze B2, DigitalOcean Spaces, and
other S3-compatible stores that satisfy the consistency and API contract below.

## Constraints

1. **Zero tokio.** This is a hard stack invariant. `aws-sdk-s3`, `rusoto`, and
   `aws-sigv4` are disqualified. `aws-sigv4` v1.4.5 pulls `tokio` through
   `aws-credential-types -> aws-smithy-async -> tokio`, even with
   `--no-default-features --features sign-http`. The S3 client uses `cyper`
   0.8 for HTTP and a hand-rolled SigV4 signer. The crypto/util deps needed
   for hand-rolled SigV4 (`sha2`, `hmac`, `hex`, `percent-encoding`, `url`,
   `base64`) are already in the workspace.
2. **Pre-launch, no back-compat scaffolding.** Local filesystem backends remain
   first-class dev backends, not compatibility shims. Do not add deprecated
   aliases, migration shims, detect-and-warn paths, legacy modes, or
   creator-code scanners. URL-vs-bare-path parsing is config ergonomics and is
   allowed.
3. **Existing traits are the starting point, not a compatibility boundary.**
   `BlobStore::put_blob_stream` takes a single-pass synchronous
   `&mut dyn std::io::Read`, verifies SHA-256 while consuming it, and returns
   `PutOutcome::{Wrote,Deduped}`. `Backend::list` currently returns an
   unpaginated `Vec<ListEntry>`. ISS-32 may deliberately break these internal
   traits, but every impl, caller, fixture, and reference doc changes in the
   same patch.
4. **No hidden trait defaults.** If the gateway needs a streaming refill method
   on `BlobStore`, add it as a required trait method and update every impl and
   test stub in the same patch. Do not hide it behind a buffered default method.
5. **Content-addressing integrity.** Bundle blob keys are SHA-256 hex strings.
   `get_blob`, dedup hits, and gateway refill must verify
   `sha256(bytes) == hash` before exposing bytes or publishing a disk-cache
   file.
6. **Size caps are enforced at the allocation boundary.** Bundle blobs use
   `MAX_BLOB_BYTES` (16 MiB) and manifests use `MAX_MANIFEST_BYTES` (1 MiB).
   `env.storage.put` and `env.storage.get` are fully buffered today, so
   encoded and decoded caps must be enforced before base64 decode/encode and
   before any backend allocates a response `Vec`.
7. **Single-pass PUT reality.** A single-pass reader cannot be hashed before
   signing/uploading unless we buffer, write a verified temp file, or implement
   SigV4 signed chunked streaming. V1 does **not** implement streaming SigV4.
8. **S3 ListObjectsV2 is XML.** There is no XML parser in the workspace today.
   Add `quick-xml` to `[workspace.dependencies]` and consume it from
   `compio-s3` via `workspace = true`; it is a small, pure parser with no async
   runtime dependency.
9. **S3 time formats need concrete parsers/formatters.** SigV4 needs UTC
   `x-amz-date` (`YYYYMMDD'T'HHMMSS'Z'`) and credential-scope date
   (`YYYYMMDD`). `GetObject`/`HeadObject` `Last-Modified` is HTTP-date.
   `ListObjectsV2` `LastModified` is RFC3339-like UTC.
10. **cyper clients must not cross compio threads.** Existing runtime/gateway
    code documents that `cyper::Client` is per-thread because its connector
    uses `SendWrapper`. `S3Client` handles are config/credential handles only;
    a live `cyper::Client` is created and used on the current compio thread.
11. **cyper pooling is not a proven dirty-connection barrier.** Every S3
    response, including PUT/DELETE error responses, can carry a body. V1 must
    either drain under a cap or drop the request-scoped client for every
    operation before another request can reuse that connection. V1 chooses
    request-scoped clients for all S3 operations.

## Architecture

```text
            +-----------------------------+
            |  crates/compio-s3 (new)     |  cyper + hand-rolled SigV4
            |  compio-native S3 client    |  GET/PUT/HEAD/DELETE/LIST
            +--------------+--------------+
                           |
      +--------------------+--------------------+
      v                    v                    v
+---------------+  +----------------+  +--------------------+
| S3BlobStore   |  | Gateway disk   |  | plugin-storage::S3  |
| bundle blobs  |  | cache refill   |  | env.storage Backend |
| + manifests   |  | from remote    |  |                    |
+---------------+  +----------------+  +--------------------+
```

### 1. `crates/compio-s3`

A small bespoke client, matching the discipline of `compio-postgres` and
`compio-redis`: explicit modules, explicit errors, bounded responses, no SDK
dependency hidden under the hood.

Module shape:

- `config.rs`: `S3Config`, URL parsing, endpoint/addressing style, canonical
  prefix, provider profile, checksum mode, SSE mode, timeouts, response caps.
- `credentials.rs`: resolved credentials value; no provider chain and no
  metadata-service calls in v1.
- `clock.rs`: injectable UTC clock and date formatting for SigV4 tests.
- `signer.rs`: hand-rolled SigV4 canonical request and authorization header.
- `client.rs`: `S3Client` methods over request-scoped `cyper::Client`.
- `list_xml.rs`: `ListObjectsV2` XML parser using `quick-xml`.
- `error.rs`: typed errors and retryability.

Dependency ownership: root `Cargo.toml` remains the single dependency-version
table. Add these to `[workspace.dependencies]`; `compio-s3` uses
`{ workspace = true }`.

```toml
quick-xml = "0.38"
time = { version = "0.3", default-features = false, features = ["std", "formatting", "parsing"] }
httpdate = "1"
```

They are pure parsing and formatting crates; dependency-tree CI must prove they
do not pull an async runtime. `zeroship-plugin-storage` depends on `compio-s3`
only behind its `s3` feature.

Operational rules:

- The live HTTP client is request-scoped and current-thread only. `S3Client`
  may be cloned/shared across `Send + Sync` trait objects because it stores only
  immutable config, credentials, and clock handles in `Arc`-backed std-sync
  state. V1 has no shared client-side limiter. If a limiter is added later, it
  must be `Send + Sync`, must not block a compio thread while waiting, and must
  not store live `cyper::Client` values.
- `RequestBuilder::send()` is wrapped in `compio::time::timeout`; this is the
  header-send wall timeout and covers DNS/connect/TLS/request upload/response
  headers as exposed by cyper's public API. cyper does not expose separate
  connect/read/write timeout knobs.
- Response bodies are consumed through the stream path, not unbounded
  `bytes()`, with both a per-chunk read timeout and a total body timeout.
- Every buffered response has a cap: caller-supplied cap for object GET,
  configured XML page cap for LIST, and a small error-body cap for diagnostics.
- On send timeout, body timeout, cancellation, cap breach, partial-read error,
  or any non-drained error response, drop the response and request-scoped client.
- If future work enables pooled client reuse, tests must prove that timeout,
  cancellation, over-cap abort, partial-read, and error-body paths do not return
  a dirty connection to the pool.

Client surface:

```rust
pub struct S3Client { /* Arc<S3Config>, Arc<S3Credentials>, clock */ }

pub struct S3ObjectMeta {
    pub len: u64,
    pub content_type: Option<String>,
    pub last_modified: std::time::SystemTime,
    pub user_sha256: Option<String>,
}

pub struct PutResult {
    pub e_tag: Option<String>,
    pub version_id: Option<String>,
}

pub struct ListPage {
    pub entries: Vec<S3ListEntry>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

impl S3Client {
    pub async fn head_object(&self, key: &str) -> Result<Option<S3ObjectMeta>, S3Error>;
    pub async fn get_object(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<(bytes::Bytes, S3ObjectMeta), S3Error>;
    pub async fn get_object_to_file(
        &self,
        key: &str,
        out: &compio::fs::File,
        expected_size: Option<u64>,
        max_bytes: u64,
        expected_sha256: Option<&str>,
    ) -> Result<S3ObjectMeta, S3Error>;
    pub async fn put_bytes(&self, key: &str, body: &[u8], opts: PutOptions<'_>) -> Result<PutResult, S3Error>;
    pub async fn delete_object(&self, key: &str) -> Result<(), S3Error>;
    pub async fn list_objects_v2(&self, prefix: &str, continuation: Option<&str>) -> Result<ListPage, S3Error>;
}
```

`put_stream` is intentionally absent from v1. A future streaming API must
choose between SigV4 chunk signing and provider-specific unsigned-payload
semantics; the current traits do not provide a `Send + 'static` stream or a
second pass.

`get_object_to_file` writes only to an already-open temp file supplied by the
caller. The S3 client never receives a raw path it can truncate, recreate, or
publish. It compares `Content-Length` when present, rejects a length larger
than `max_bytes` or different from `expected_size`, stops reading once observed
bytes exceed the cap, and hashes while streaming when `expected_sha256` is set.

### 2. SigV4

The signer is hand-rolled. Required behavior:

- Canonical request fields: method, canonical URI, canonical query string,
  canonical headers, signed headers, hashed payload.
- Do **not** normalize S3 object paths. S3 treats repeated slashes as part of
  the key.
- URI-encode with S3's SigV4 rules: unreserved bytes stay literal, spaces are
  `%20`, hex digits are uppercase, and `/` is preserved inside the object-key
  portion.
- Query parameters are URI-encoded individually and sorted after encoding.
- Header names are lowercased and sorted; `host`, `x-amz-date`, and
  `x-amz-security-token` when present are signed. Sign all `x-amz-*` headers we
  send, including `x-amz-content-sha256`, even though S3 treats that header
  specially.
- PUT signs the **actual SHA-256 payload hash** and sends the same value in
  `x-amz-content-sha256`. `UNSIGNED-PAYLOAD` is not used for PUT in v1.
- GET/HEAD/DELETE use the empty-body SHA-256 hash. `UNSIGNED-PAYLOAD` may be
  considered later only with an explicit TLS-only integrity rationale.
- `clock.rs` formats `x-amz-date` and credential scope from the same injected
  UTC instant. Unit tests cover 23:59:59 -> 00:00:00 UTC day-boundary changes
  so the scope date cannot diverge from `x-amz-date`.

Tests use AWS-published SigV4 examples/test vectors, including the chunked
streaming examples as negative/fixture context even though V1 does not stream
PUT bodies. Local tests cover spaces, repeated slashes, `%2F`, `%`, `?`, `#`,
Unicode UTF-8 bytes, empty query values, response header overrides, temporary
credentials, and R2's `region=auto`.

### 3. XML, dates, and key decoding for LIST

`ListObjectsV2` responses are XML and a page contains at most 1,000 keys.
Requests set `encoding-type=url`. The parser:

- reads top-level `IsTruncated` and `NextContinuationToken`;
- reads repeated `Contents` entries;
- reads per object `Key`, `Size`, and `LastModified`;
- percent-decodes URL-encoded `Key` bytes after XML parsing;
- verifies decoded keys are valid UTF-8 and start with the configured internal
  prefix;
- strips the internal prefix before returning storage keys;
- parses `LastModified` with `time`'s RFC3339 parser.

Unknown XML fields are ignored. Missing required `Key`, missing/invalid `Size`,
missing/invalid `LastModified`, invalid percent encoding, or invalid UTF-8 is
`S3Error::InvalidResponse`.

`GetObject`/`HeadObject` parse `Last-Modified` with `httpdate`. A missing
`Last-Modified` on a successful object `GET` or `HEAD` is `InvalidResponse`
because `ObjectMeta.modified_at` is mandatory.

### 4. Error taxonomy and retries

`compio-s3::S3Error` must keep enough shape for callers to map correctly:

- `NotFound`: HTTP 404.
- `Auth`: HTTP 401/403, credential/signature errors.
- `PreconditionFailed`: HTTP 412 from conditional PUT.
- `Conflict`: HTTP 409 from conditional write races.
- `Retryable`: 429, 500, 502, 503, 504, and compio/cyper timeouts.
- `InvalidResponse`: malformed XML, missing headers, bad content length,
  checksum/header mismatch.
- `TooLarge`: response or request exceeds configured caps.
- `Transport`: cyper/HTTP/TLS/socket errors.

Retry rules:

- Bounded automatic retries are allowed for idempotent `GET`, `HEAD`, `LIST`,
  and `DELETE`.
- Content-addressed bundle `PUT` is conditional (`If-None-Match: *`) and uses a
  buffered body. On 412, re-GET and hash before returning `Deduped`. On 409 or
  transport ambiguity after bytes may have reached S3, re-GET first; if the
  object exists and hashes correctly, return `Deduped`; if it is absent, a
  bounded retry of the same conditional PUT is allowed.
- Mutable `env.storage.put` is an overwrite API. Do **not** automatically retry
  it after `send()` has started or after any timeout/transport error where
  bytes may have reached S3. Return the error and let the caller decide whether
  to repeat a last-writer-wins write.

Mappings:

- Bundle `get_blob`: 404 -> `BlobError::NotFound`; hash mismatch ->
  `BlobError::HashMismatch`; auth/config/retryable/transport ->
  `BlobError::Backend`.
- `env.storage get`: 404 -> `Ok(None)`.
- `env.storage delete`: S3 has no single unversioned delete operation that
  returns the same atomic existence bool as `LocalFs::remove_file`. V1 uses
  HEAD-before-DELETE and documents weaker concurrency semantics: `true` means
  "this caller observed the object before issuing DELETE and DELETE was
  accepted", not "this caller was the unique deleter". Two concurrent callers
  can both return `true`.
- `env.storage` flattens backend failures to `String`, so S3 mappings use
  stable class prefixes and may append diagnostic detail after `:`.
  - `Auth` -> `storage: s3 auth during <op>`.
  - `TooLarge` -> `storage: s3 too-large during <op>`.
  - `InvalidResponse` -> `storage: s3 invalid-response during <op>`.
  - `Retryable` timeout/status -> `storage: s3 retryable during <op>`.
  - `Conflict`/`PreconditionFailed` on mutable writes -> `storage: s3 conflict during <op>`.
  - `Transport` -> `storage: s3 transport during <op>`.
  - unexpected successful-status contract failures -> `storage: s3 invalid-response during <op>`.
  These class strings are part of the backend contract for put/get/delete/list
  tests; provider-specific request IDs may be included only after the stable
  prefix.

### 5. `S3BlobStore` for deploy blobs

Key layout under configured prefix:

- blobs: `{prefix}/blobs/{sha256}`
- manifests: `{prefix}/manifests/{app_id}/{deploy_hash}.json`

Break `BlobStore` deliberately in the same patch:

```rust
#[async_trait::async_trait(?Send)]
pub trait BlobStore: Send + Sync + std::fmt::Debug {
    async fn get_blob_to_file(
        &self,
        hash: &str,
        out: &compio::fs::File,
        expected_size: Option<u64>,
        max_bytes: u64,
    ) -> Result<u64, BlobError>;

    async fn delete_app_manifests(&self, app_id: &uuid::Uuid) -> Result<(), BlobError>;

    // existing required methods...
}
```

No default implementation is added. `LocalDiskBlobStore`, `S3BlobStore`, every
gateway/control/worker test stub, and every caller must be updated.

`put_blob_stream`:

1. Validate `hash` using `validate_hash_format`.
2. If `HEAD`/metadata says the key exists, metadata is not proof of bytes.
   Before returning `PutOutcome::Deduped`, GET the existing blob under
   `MAX_BLOB_BYTES`, hash it, verify `sha256(bytes) == hash`, then drain the
   caller's reader. If verification fails, return backend corruption. Apply the
   same rule to `LocalDiskBlobStore`: existing local bytes must be size/hash
   verified before `Deduped`.
3. Read the single-pass `std::io::Read` once into a bounded buffer while
   hashing and counting bytes. Reject if `expected_size > MAX_BLOB_BYTES`,
   observed bytes exceed `expected_size`, observed bytes exceed
   `MAX_BLOB_BYTES`, observed size differs from `expected_size`, or the
   computed SHA-256 differs from `hash`.
4. PUT with explicit `Content-Length`, signed payload hash,
   `If-None-Match: *`, `x-amz-meta-sha256: <hash>`, and
   `Content-Type: application/octet-stream`. Add `x-amz-checksum-sha256` only
   when `checksum=sha256`; the header value is base64-encoded SHA-256 bytes,
   and byte hashing remains mandatory because checksum/user metadata is not
   trusted proof of content-addressing integrity. `checksum`
   defaults come from the provider profile (`aws`/`minio`: `sha256`,
   `r2`/`generic`: `none`) and can be overridden explicitly.

`get_blob`:

- GET the object with a cap of `MAX_BLOB_BYTES`.
- Parse `Content-Length`; reject length over cap.
- Hash returned bytes and return `BlobError::HashMismatch` on mismatch before
  exposing bytes to the worker.

`get_blob_to_file`:

- Caller supplies an open temp file created with `create_new`.
- `LocalDiskBlobStore` copies bytes from its local blob path into the supplied
  open temp file, then hashes and size-checks. It does not claim to hard-link
  through the file handle; if a future local-cache fast path wants a hard-link,
  it needs a separate path-publish API that receives source and destination
  paths.
- `S3BlobStore` streams the response body into that file while hashing. It
  compares `Content-Length` when present, aborts once observed bytes exceed
  `expected_size` or `max_bytes`, and returns success only after SHA-256 matches
  `hash`.

`has_blob`:

- HEAD -> bool. Auth/config errors remain errors.

`local_path`:

- Pure S3 returns `None`. Gateway hot-path preservation lives in the disk-cache
  refill changes below.

Manifests:

- Manifest writes deliberately change from today's local temp-rename overwrite
  semantics to immutable-key semantics. This is a pre-launch internal contract
  change; update `LocalDiskBlobStore`, `S3BlobStore`, callers, tests, and
  reference docs in the same patch.
- `put_manifest` rejects `json.len() > MAX_MANIFEST_BYTES` before upload.
- `get_manifest` uses a cap of `MAX_MANIFEST_BYTES`, compares
  `Content-Length` when present, and rejects oversized responses.
- PUT `application/json`.
- Use immutable cache headers because the key includes `deploy_hash`:
  `Cache-Control: public, max-age=31536000, immutable`.
- Use conditional PUT (`If-None-Match: *`) when writing a new manifest key.
  Identical replay may be treated as success after capped GET/byte comparison;
  divergent existing content is a backend error. HTTP 409 or transport
  ambiguity after bytes may have reached the provider is resolved by capped GET:
  identical existing JSON is success, absent content may be retried within the
  bounded idempotent retry budget, and divergent content is `BlobError::Backend`.
  A provider/profile that cannot honor conditional PUT is rejected at
  config/probe time; there is no unsafe overwrite fallback for manifests.
- GET 404 maps to `BlobError::NotFound`.

App purge:

- Delete `AppState.vfs`, `PurgeError::Vfs`, the synchronous `BundleStore`
  construction in control, and the `state.vfs.delete` step in `purge_app`.
- Replace it with `BlobStore::delete_app_manifests(app_id)`, called before the
  registry cascade. `LocalDiskBlobStore` deletes
  `<root>/manifests/<app_id>/`. `S3BlobStore` lists
  `{prefix}/manifests/{app_id}/` and deletes those manifest objects.
- `S3BlobStore::delete_app_manifests` loops `ListObjectsV2` pages until
  `IsTruncated=false`, using `NextContinuationToken` exactly as returned. Each
  page is capped by the configured XML body cap and parsed as XML; invalid XML
  is a backend error.
- Every listed manifest object is deleted with bounded idempotent retries for
  retryable status/transport failures. A per-object 404 during delete is
  success because another purge attempt may have already removed it.
- Partial failure behavior is strict: if any page list or object delete fails
  after retries, `delete_app_manifests` returns `BlobError::Backend` with the
  number of objects deleted and the first failing key/status in diagnostics.
  `purge_app` then aborts before `registry.delete_app`, preserving today's
  "artifact purge first, DB cascade second" ordering.
- The method is idempotent. Empty prefix, repeated calls after full success,
  and objects disappearing between LIST and DELETE all return success.
- Content-addressed blobs under `{prefix}/blobs/` are not app-owned and are not
  deleted during app purge; shared blob GC/refcounting is a separate design.
- `NotFound`/empty manifest prefix is success. Auth/config/transport errors
  block purge before registry deletion.

This cleanup is part of the ISS-32 storage contract, not a later prerequisite.
Control accepts `s3://` only in the same patch that removes or replaces the
legacy VFS field.

### 6. Gateway disk-cache refill

Do not introduce a `CachedBlobStore` that assumes gateway uses
`BlobStore::local_path`; it does not. The current gateway path is:

- `BlobCache` memory hit.
- `DiskBlobCache::local_path` mmap/stream hit.
- `store.get_blob(hash)` on miss, then `DiskBlobCache::insert`.

That miss path buffers the full object today. Change the gateway cache API:

- `async fn DiskBlobCache::reserve_temp(&self, hash: &str) -> io::Result<DiskBlobTemp>`
  creates the hash shard directory, opens a unique temp path under the cache
  root with `compio::fs::OpenOptions::create_new(true).write(true)`, and
  returns a guard that owns both the temp path and the already-open
  `compio::fs::File`.
- `DiskBlobTemp` unlinks its temp file on drop unless published.
- Gateway passes `temp.file()` to `BlobStore::get_blob_to_file`; the blob store
  and S3 client never receive a raw path.
- `async fn DiskBlobCache::publish_temp(&self, hash: &str, temp: DiskBlobTemp, verified_size: u64) -> io::Result<PathBuf>`
  first `sync_all`s and closes the temp file, then publishes only a temp file
  whose bytes were already size/hash verified by the store. It returns the final
  path that the static response should serve.
- The static path passes the exact asset or variant size from the manifest as
  `expected_size` and `MAX_BLOB_BYTES` as `max_bytes`.

Publish conflict semantics:

- Per-process singleflight keyed by blob hash collapses concurrent cold misses
  in one gateway process.
- `publish_temp` takes the cache lock, checks whether the hash is already in
  the LRU and the final path exists, discards the temp and promotes the
  existing entry if so.
- Publishing must use a no-clobber primitive. First try
  `std::fs::hard_link(temp_path, final_path)` then unlink temp; temp and final
  are under the same cache root, so cross-device should not occur. If hard-link
  is unavailable, use an `open(final, create_new)` copy from the verified temp
  into the newly-created final file. Do not fall back to overwrite `rename`.
  If the final path already exists, verify the existing file's size/hash before
  trusting it. If valid, discard temp and insert/promote LRU bookkeeping. If
  invalid, unlink the corrupt final path and retry publishing the verified temp
  with the same no-clobber primitive.
- Byte accounting is updated exactly once for the final file that wins. If an
  existing tracked entry is replaced because it was corrupt, subtract old size
  before adding the verified size.
- Multi-process correctness comes from temp uniqueness, no-clobber publish, and
  final-file verification. Duplicate downloads are acceptable; partial reads
  are not.

### 7. `plugin-storage::S3` for `env.storage`

Object key layout under configured prefix:

`{prefix}/apps/{app_id}/{bucket}/{key}`

The logical `app_id`, `bucket`, and `key` are UTF-8 strings that passed backend
validation. The stored S3 object key is the UTF-8 byte sequence formed by
joining internal prefix segments with literal `/`. Request URL construction is
responsible for SigV4/S3 URI encoding:

- spaces -> `%20`;
- `%` -> `%25`, so a logical key containing `%2F` remains the three bytes
  `%`, `2`, `F` and does not become a slash;
- `?` -> `%3F`;
- `#` -> `%23`;
- Unicode is encoded as UTF-8 bytes and then percent-encoded where required;
- logical `/` remains a slash separator inside the object key.

Deliberately update the existing `Backend` contract so the allocation cap is
visible to every implementation:

```rust
async fn get(
    &self,
    app_id: &str,
    bucket: &str,
    key: &str,
    max_bytes: u64,
) -> Result<Option<(Vec<u8>, ObjectMeta)>, String>;
```

Implement behavior:

- `put`: add `ZEROSHIP_STORAGE_MAX_OBJECT_BYTES` (default 16 MiB). The native
  callback rejects base64 strings before calling `base64::decode` when their
  encoded length cannot fit under the decoded cap. Use the padded STANDARD
  alphabet only; reject whitespace and unpadded input. Let `cap` be decoded
  bytes, `max_encoded = cap.checked_add(2)?.checked_div(3)?.checked_mul(4)?`,
  and reject if `encoded_len > max_encoded` or if any checked arithmetic
  overflows. For syntactically padded input, `decoded_exact =
  (encoded_len / 4) * 3 - padding_count`, where `padding_count` is 0, 1, or 2;
  reject if `encoded_len % 4 != 0`, padding is malformed, or
  `decoded_exact > cap`. Then decode and recheck `Vec::len() <= cap` before
  dispatching to the backend. This cap applies to LocalFs and S3. V1 keeps
  buffered PUT. Multipart upload is out of scope until the SDK/native API grows
  streaming writes.
- `get`: callback passes the decoded cap into `Backend::get`. LocalFs checks
  file metadata size before `Vec::with_capacity`; S3 checks `Content-Length`
  before reading and aborts over-cap streams. The callback rechecks decoded
  length before base64 encoding back to JS and computes the response encoded
  length with the same checked `4 * ceil(n / 3)` formula before allocating the
  encoded string.
- `delete`: use HEAD-before-DELETE for S3 and document the weakened concurrency
  semantics described in the error mapping.
- `list`: keep the current unpaginated `Vec<ListEntry>` contract for v1 by
  looping `ListObjectsV2` pages internally until `IsTruncated=false`. Configure
  a hard cap (`max_list_entries`, default 10,000) and return an error if
  exhausted; this is the explicit scaling risk of retaining the current SDK
  shape. After aggregating and stripping the internal prefix, sort entries by
  returned logical key before returning, matching LocalFs determinism instead
  of provider-order passthrough.

Validation:

- Define one shared grammar for both object operations and list operations,
  then update `validate_object_coords` and the new `validate_list_coords` to
  call the same helpers. S3 and LocalFs must accept the same app/bucket/key
  space; writes must not be able to create buckets that list rejects.
- Exact rules: `app_id` and `bucket` must be non-empty. `app_id` rejects `/`,
  `\`, NUL, exact `.`, exact `..`, and any `..` substring. `bucket` rejects
  `/`, `\`, NUL, exact `.`, exact `..`, and any `.` character; this deliberately
  tightens today's `validate_object_coords`, which currently accepts dotted or
  backslash-containing bucket names.
- Object `key` must be non-empty, may contain nested `/` segments, and rejects
  leading `/`, trailing `/`, repeated separators, `.`/`..` segments,
  backslashes, and NUL.
- List `prefix` uses the same segment grammar as `key`, except it may be empty
  and may end in one `/`. Prefix filtering happens after validation.
- Shared backend conformance tests prove LocalFs and S3 agree on accepted and
  rejected prefixes.

Content type and LocalFs sidecars:

- Choose parity now: LocalFs stores content type metadata in the same patch
  instead of accepting divergent conformance.
- Change LocalFs layout pre-launch to keep metadata outside the data tree:
  objects live under `<root>/objects/<app_id>/<bucket>/<key>` and metadata
  sidecars under `<root>/metadata/<app_id>/<bucket>/<key>.json`. `list` walks
  only `<root>/objects/...`, so sidecars cannot appear as user objects.
- No migration is needed; there are no production objects.
- `LocalFs::put` stages a unique temp object and a unique temp sidecar with
  `create_new`. The sidecar contains `content_type`, `size`, `sha256`, and a
  file-generation marker captured from the final object after commit. On Linux
  the marker is `(dev, ino, mtime_nsec)` from `std::os::unix::fs::MetadataExt`;
  on any platform where a stable marker is unavailable, LocalFs ignores
  sidecars rather than risking stale metadata.
- `LocalFs::put` uses an object-first protocol. Write/fsync the temp object,
  rename the temp object over the final object path, stat the final object,
  write/fsync the temp sidecar with the final object's generation marker, then
  rename the sidecar over the final sidecar path. If object rename fails, no
  new sidecar is written. If sidecar write/rename fails after the object commit,
  the object remains visible but metadata falls back to `content_type: None` or
  the previous sidecar only if its generation marker still matches. A same-size,
  same-hash rewrite with a different content type cannot publish new metadata
  unless the object rename succeeded.
- `LocalFs::get` reads the object, hashes it, stats it, and uses the sidecar
  content type only if sidecar size/hash and generation marker match the
  current object. Stale or malformed sidecars are ignored and may be removed
  best-effort.
- `LocalFs::delete` removes the object first and then removes the sidecar
  best-effort. A stale sidecar without an object is never listed and never
  makes `get` return an object.

### 8. Consistency contract

The backend assumes:

- PUT is read-after-write consistent for GET/HEAD.
- DELETE is immediately visible to GET/HEAD.
- LIST reflects completed PUT/DELETE operations strongly enough for a worker
  fleet to observe object changes without read-repair.

AWS S3 satisfies strong read-after-write consistency for PUT/DELETE, GET/HEAD,
metadata, and LIST. R2 documents strong global consistency for read-after-write,
metadata, delete, and object listing through its S3 API path. MinIO is the CI
target, but production support for any S3-compatible provider requires a
multi-worker put/read/list/delete qualification test against that provider
before it is marked supported. No retry/read-repair abstraction is added in v1.

### 9. Encryption

Transport:

- Production S3 endpoints require HTTPS. Plain HTTP is allowed only for local
  MinIO under an explicit `dev_http=true` config flag, and the parser enforces
  that the HTTP endpoint host is loopback/localhost.

At rest launch policy:

- Default is `sse=none`, meaning the client sends no SSE headers and relies on
  bucket/provider default encryption. This keeps AWS/R2/MinIO behavior
  provider-neutral by default.
- AWS S3 encrypts new uploads with SSE-S3 by default. Use `sse=sse-s3`
  (`x-amz-server-side-encryption: AES256`) only when a bucket policy requires
  the explicit header.
- `sse=sse-kms:<key-id-or-alias>` is AWS-default-endpoint only in v1 and signs
  `x-amz-server-side-encryption: aws:kms` plus
  `x-amz-server-side-encryption-aws-kms-key-id`.
- R2 encrypts objects and metadata at rest automatically; `provider=r2` rejects
  `sse=sse-s3` and `sse=sse-kms:*` because R2's S3 API marks those AWS SSE
  headers unsupported. SSE-C is out of scope for v1.
- MinIO support is provider/config dependent; the E2E harness covers only the
  mode documented for local testing.
- Tests assert the emitted PUT headers for default/no-header, SSE-S3, and
  SSE-KMS modes.

### 10. Single-PUT and multipart

S3 single PUT supports objects up to 5 GiB; multipart is the required shape for
larger uploads and AWS recommends considering multipart once objects reach
large-object territory. Zeroship v1 caps bundle blobs and `env.storage` objects
far below that limit because both paths are currently fully buffered for PUT
signing or JS base64 transport. Multipart upload is not an S3-client-only
follow-up; it requires a native and SDK streaming redesign.

## Config

Use one parser for object-store locations and one credentials resolver across
control, gateway, worker, and `zeroship serve`. Local paths remain first-class
dev configuration, not legacy fallbacks.

### Shared parser

Add `zeroship_core::object_store::StoreUrl`:

```rust
pub enum StoreUrl {
    Local(std::path::PathBuf),
    S3(S3UrlParts),
}
```

`S3UrlParts` contains non-secret bucket, prefix, provider profile, endpoint,
region, addressing style, `dev_http`, checksum mode, SSE mode, and optional
list caps. `compio-s3` converts it to `S3Config` after credentials and timeout
defaults are resolved.

Accepted values:

```text
./bundles
file:///var/lib/zeroship/bundles
s3://bucket/prefix?region=us-east-1
s3://bucket/prefix?provider=r2&endpoint=https://<acct>.r2.cloudflarestorage.com&region=auto&style=path
s3://bucket/prefix?provider=minio&endpoint=http://127.0.0.1:9000&region=us-east-1&style=path&dev_http=true
```

Parser rules:

- `s3://bucket`, `s3://bucket/`, and `s3://bucket//` are not equivalent:
  empty prefix is allowed for the first two, but repeated prefix separators are
  rejected rather than normalized.
- The single URL path slash before the prefix is syntax and is removed. A
  trailing slash on a non-empty prefix is trimmed. Empty prefix is stored as
  `None`.
- Prefix path segments are percent-decoded once as UTF-8 for config
  ergonomics. Reject invalid percent encoding, invalid UTF-8, `%2F`/`%2f`
  inside a segment, empty segments after trimming, `.`, `..`, backslashes, and
  repeated `/`.
- Unknown or duplicate query parameters are errors.
- `provider`: `aws`, `r2`, `minio`, or `generic`. If omitted, infer `aws`
  when no custom endpoint is present or the endpoint host is an AWS S3 host,
  infer `r2` for `*.r2.cloudflarestorage.com`, infer `minio` for loopback
  `dev_http=true`, otherwise use `generic`.
- `region`: required except `region=auto`, which is accepted only for
  `provider=r2`. `region=auto` is rejected for AWS, MinIO, and generic custom
  endpoints.
- `endpoint`: optional for AWS, required for R2/MinIO/B2/Spaces.
- `style`: `virtual` or `path`; default `virtual` for AWS, `path` for custom
  endpoints unless overridden.
- Virtual-hosted style requires a bucket name safe for the chosen endpoint and
  TLS hostname. If the bucket contains dots or otherwise cannot be safely used
  as a virtual-host label, the parser requires `style=path`.
- `sse`: `none`, `sse-s3`, or `sse-kms:<key-id-or-alias>`.
- `checksum`: `none` or `sha256`. Defaults by provider profile are
  `aws=sha256`, `minio=sha256`, `r2=none`, `generic=none`. R2 currently rejects
  the AWS checksum algorithm headers used for `x-amz-checksum-sha256`, so R2
  qualification must keep this default unless Cloudflare changes that support.
- `dev_http=true` requires `endpoint=http://...` and the endpoint host must be
  loopback or localhost (`localhost`, `127.0.0.0/8`, or `::1`). Non-loopback
  plaintext S3 endpoints are rejected in v1 rather than hidden behind a broad
  production escape hatch.
- `max_list_entries`: optional cap for plugin-storage list when reused there.

### Blob store

Existing flag/env on all three production binaries:

- `zeroship-control --blob-store` / `BLOB_STORE`
- `zeroship-worker --blob-store` / `BLOB_STORE`
- `zeroship-gate --blob-store` / `BLOB_STORE`

All three binaries parse the value through `StoreUrl` and build the same
`Arc<dyn BlobStore>` shape:

- `Local(path)` -> `LocalDiskBlobStore`.
- `S3(config)` -> `S3BlobStore`.

Control no longer constructs `LocalFs`/`BundleStore` from this string. The VFS
field is deleted before `s3://` is accepted by control.

### `env.storage`

Runtime storage config applies to both production worker and `zeroship serve`:

- `zeroship-worker --storage-url` / `ZEROSHIP_STORAGE_URL`
- `zeroship serve` reads `ZEROSHIP_STORAGE_URL`; when unset, it uses the
  explicit local dev default `file://.zeroship/storage`.

Accepted values:

```text
# Empty in production worker means env.storage is absent.

/var/lib/zeroship/storage
file:///var/lib/zeroship/storage
s3://bucket/storage?region=us-east-1
s3://bucket/storage?provider=r2&endpoint=https://<acct>.r2.cloudflarestorage.com&region=auto&style=path
```

Exact code shape:

- Replace `WorkerCli.storage_root` with `storage_url`.
- Replace `WorkerConfig.storage_root: Option<PathBuf>` with
  `storage_backend: Option<StorageBackendConfig>`.
- Replace `KernelConfig.storage_root: Option<PathBuf>` in `crates/worker/src/cache.rs`
  with the same `StorageBackendConfig`.
- `create_plugins()` matches `StorageBackendConfig::{Local,S3}` and constructs
  `StoragePlugin::local(root)` or `StoragePlugin::s3(config)`.
- `zeroship serve` uses the same parser and constructor path.
- Remove `--storage-root`, `ZEROSHIP_STORAGE_ROOT`, and the
  `StoragePlugin::new` alias in the same patch that adds `--storage-url`; do
  not keep aliases.

### Credentials

V1 supports one S3 identity per process. If a binary configures both remote
deploy blobs and remote `env.storage`, the same resolved credentials must have
permission to both buckets/prefixes. Per-backend credential references are a
future config feature, not implied by ISS-32.

Add explicit secret fields to `SecretSection`:

```rust
pub s3_access_key_id: Option<String>,
pub s3_secret_access_key: Option<String>,
pub s3_session_token: Option<String>,
```

Wire the same CLI/env names on control, gateway, worker, and `zeroship serve`
where applicable:

- `--s3-access-key-id` / `AWS_ACCESS_KEY_ID`
- `--s3-secret-access-key` / `AWS_SECRET_ACCESS_KEY`
- `--s3-session-token` / `AWS_SESSION_TOKEN`

Resolve each through `obtain_secret`, with `[secrets]` acting as the overlay
when CLI/env is empty. If any configured S3 backend lacks required
credentials, `--check-config` fails and normal boot refuses to start.

### `--check-config`

Never print credential values or full URLs. Print parsed, non-secret fields.

Control report keys:

- `blob_store_kind`: `local` or `s3`
- local: `blob_store_path`
- s3: `blob_store_bucket`, `blob_store_prefix`, `blob_store_endpoint`,
  `blob_store_provider`, `blob_store_region`, `blob_store_style`,
  `blob_store_checksum`, `blob_store_sse`, `blob_store_dev_http`
- `s3_access_key_id_configured`, `s3_secret_access_key_configured`,
  `s3_session_token_configured`
- existing control fields such as `deploy_tmp_dir`, `workers_count`, and
  secret booleans remain

Gateway report keys:

- same blob-store keys and S3 credential booleans as control
- existing cache keys remain: `blob_cache_mem_mb`, `blob_cache_disk_gb`,
  `blob_cache_disk_root`

Worker report keys:

- same blob-store keys and S3 credential booleans as control
- `storage_configured`
- `storage_kind`: `absent`, `local`, or `s3`
- local storage: `storage_path`
- s3 storage: `storage_bucket`, `storage_prefix`, `storage_provider`,
  `storage_endpoint`, `storage_region`, `storage_style`, `storage_checksum`,
  `storage_sse`, `storage_dev_http`, `storage_max_object_bytes`,
  `storage_max_list_entries`

`zeroship serve` startup logging follows the same redaction discipline: print
storage kind and non-secret local path/bucket/prefix/endpoint, never secrets.

## Testing

- **SigV4 unit tests:** AWS examples/test vectors for GET, PUT, query strings,
  temporary credentials, canonical URI edge cases, signed `x-amz-*` headers,
  UTC day-boundary formatting, and streaming SigV4 fixture coverage that proves
  v1 does not accidentally emit `STREAMING-AWS4-HMAC-SHA256-PAYLOAD`.
- **Zero-tokio dependency guard:** CI runs a script that fails if tokio appears
  below S3 crates, for example:

  ```sh
  cargo tree -i tokio -p compio-s3 >/tmp/compio-s3-tokio.txt 2>/dev/null && exit 1
  cargo tree -i tokio -p zeroship-plugin-storage --features s3 >/tmp/plugin-storage-tokio.txt 2>/dev/null && exit 1
  cargo tree -p compio-s3 | rg 'aws-sdk-s3|rusoto|aws-sigv4' && exit 1
  ```

  The script treats "tokio package not found in this dependency graph" as
  success and any inverse tree as failure.
- **URL/key encoding tests:** parser rejects unknown/duplicate params,
  repeated prefix separators, `%2F` in prefixes, invalid virtual-host buckets,
  invalid UTF-8, `region=auto` outside `provider=r2`, non-loopback
  `dev_http=true`, invalid checksum/provider combinations, and invalid
  SSE/provider combinations. Signer tests cover spaces, `%`, `%2F`, `?`, `#`,
  Unicode, and repeated slashes without path normalization.
- **XML parser tests:** sample `ListObjectsV2` XML with multiple `Contents`,
  `encoding-type=url` keys, missing optional fields, `IsTruncated=true`,
  `NextContinuationToken`, invalid size, invalid timestamp, namespaces, and
  page-size cap breaches.
- **S3 client tests:** MinIO-backed HEAD/GET/PUT/DELETE/LIST, conditional PUT
  `If-None-Match: *`, 404 mapping, auth failure mapping, response caps,
  timeout mapping, oversized `Content-Length`, over-cap streaming abort, error
  body cap/drain/drop behavior on every method, temp unlink on failure,
  checksum header behavior for `checksum=none|sha256`, and SSE header behavior
  for default/SSE-S3/SSE-KMS modes.
- **cyper lifecycle tests:** prove every operation uses current-thread
  request-scoped clients and drops them after timeout, cancellation, cap breach,
  partial-read errors, and non-drained error bodies. If pooling is introduced,
  add dirty-connection reuse regression tests before enabling it.
- **BlobStore conformance:** shared tests for `LocalDiskBlobStore` and
  `S3BlobStore`: `put_blob_stream` wrote/deduped, drains reader on dedup,
  verifies existing local/S3 bytes before deduping, rejects wrong hash/size,
  verifies on get, required `get_blob_to_file` size/hash behavior, manifest
  put/get/not-found/oversize, immutable manifest replay/divergence/409
  ambiguity behavior, app manifest purge across multiple LIST pages with
  partial delete failure, and concurrent same-hash puts.
- **Gateway cache tests:** cold miss fills a unique `create_new` temp file,
  passes an open file handle to the store, compares expected size and
  `Content-Length`, aborts/unlinks over cap, verifies hash before publish,
  handles publish conflict by promoting a valid existing file or replacing a
  corrupt untracked file, keeps byte accounting correct, singleflight collapses
  concurrent same-hash misses, and static responses do not require a full
  in-memory buffer for S3. Add an integration test that starts two gateway
  processes against the same cache root and forces same-hash cold misses,
  corrupt-final replacement, and byte-accounting reconciliation without
  overwrite-rename.
- **Backend conformance:** create the suite first; there is no existing
  `plugin-storage` parity suite. Cover callback pre-decode cap failures,
  exact padded-base64 length/padding/overflow handling, backend get cap before
  allocation, put/get metadata, LocalFs sidecar
  content type and stale-sidecar handling, sidecars never appearing in list,
  missing get, weakened delete bool concurrency semantics, list
  ordering/prefix/empty prefix, invalid coords, shared list validation, key
  encoding, and object cap failures. Run it against LocalFs and S3(MinIO).
- **E2E harness:** extend `tests/lib/e2e_stack.sh` to start MinIO, create
  buckets/prefixes, pass `--blob-store s3://...` to control/worker/gateway,
  pass `--storage-url s3://...` to worker, and pass S3 credentials. Deploy an
  app that exercises bundle dispatch/static assets and `env.storage`
  put/get/list/delete across at least two workers.
- **Provider qualification:** before calling this production-ready, run the
  same multi-worker test suite against MinIO and at least one non-MinIO target
  (AWS S3 or R2). R2 must be tested because it has provider-specific region,
  endpoint, checksum, and encryption behavior.
- **Check-config tests:** control/gateway/worker report parsed S3 config
  without secrets, fail missing credentials, reject invalid S3 URL/SSE combos,
  resolve `[secrets]` overlays for the new S3 fields, and never print raw URLs
  or credential values.

## Rollout

Pre-launch means no migration tooling and no compatibility aliases:

- Add `compio-s3` and shared `StoreUrl` parsing without adding `aws-sigv4`.
- Correct the `crates/plugin-storage/Cargo.toml` `s3` feature comment in the
  implementation patch if it still claims `aws-sigv4` adds no tokio. The
  canonical comment must name cyper + hand-rolled SigV4 and forbid
  `aws-sigv4`.
- Add the CI zero-tokio/banned-SDK dependency guard before S3 feature work can
  merge.
- Deliberately break `BlobStore` for required streaming refill and app-manifest
  purge, then update every impl/test stub in one patch.
- Delete the control legacy `BundleStore` VFS field/path in the same production
  slice that makes control accept `s3:// --blob-store`.
- Remove `StoragePlugin::new`, `--storage-root`, and `ZEROSHIP_STORAGE_ROOT`
  when touching plugin-storage config; keep `StoragePlugin::local` as the
  explicit local constructor.
- Keep local paths as the dev default.
- Update Docker Compose/runbooks to use MinIO or R2 for production-like
  multi-node runs.

No data migration is required; there are no production users or creator
objects.

## Build sequence

1. `crates/compio-s3`: config/parser, credentials, clock/date formatting,
   provider/checksum/SSE validation, error taxonomy, hand-rolled SigV4, XML
   list parser, request-scoped cyper lifecycle, timeout mechanics, dependency
   guard, unit tests, and MinIO smoke tests.
2. `zeroship-bundle`: deliberately add required `BlobStore::get_blob_to_file`
   and `delete_app_manifests`, update `LocalDiskBlobStore`, byte-verified local
   dedup, all test stubs, and conformance tests.
3. `S3BlobStore`: blob/manifests, byte-verified dedup, manifest caps,
   conditional PUT semantics, app manifest purge, and S3-backed conformance
   tests.
4. Control storage cleanup: delete `AppState.vfs`/legacy `BundleStore`, replace
   purge semantics with `delete_app_manifests`, and construct `BlobStore` from
   shared `StoreUrl` so control can write S3 deploy artifacts.
5. Gateway/worker blob config: parse `--blob-store` through `StoreUrl`, build
   `LocalDiskBlobStore` or `S3BlobStore`, and update worker bundle fetch tests.
6. Gateway cache: `DiskBlobCache` temp-file reservation/publish API, open-file
   S3 refill path, no-clobber publish conflict handling, two-process
   same-cache-root tests, singleflight by hash, and static serve tests.
7. Shared production config: add S3 credential flags/env and `[secrets]` fields,
   update `--check-config` report keys/redaction in control/gateway/worker, and
   add config tests.
8. `plugin-storage`: callback encoded/decoded caps, `Backend::get` cap
   argument, LocalFs object/metadata layout and sidecar semantics, S3 backend,
   deterministic paginated list, worker `--storage-url`, CLI dev
   `ZEROSHIP_STORAGE_URL`, remove `--storage-root`/`ZEROSHIP_STORAGE_ROOT`, and
   delete `StoragePlugin::new`.
9. E2E: MinIO harness, multi-worker deploy/static/env.storage app, provider
   qualification script/docs.
10. Docs: `blob-store.md`, `plugin-system.md`, `docker-compose.md`,
    local-dev/runbook snippets, and production credential examples.

## Open questions

1. **Gateway disk cache defaults.** Keep the current 20 GiB disk / 256 MiB mem
   defaults for S3, or lower worker/gateway dev defaults to reduce local disk
   surprise?
2. **`env.storage` object cap.** Proposed v1 default is 16 MiB decoded bytes to
   match bundle blobs and the current fully-buffered SDK/native path. Should
   the cap be lower because base64 response encoding allocates roughly 4/3 of
   the decoded payload?
3. **Paginated storage SDK.** V1 loops internally to preserve
   `list(): ListEntry[]`. Before launch, should we deliberately break the SDK
   to expose pagination and remove the `max_list_entries` scaling ceiling?
4. **Multipart/streaming storage API.** Multipart upload is not an S3-client-only
   follow-up; it requires a native and SDK streaming redesign. Decide after the
   v1 cap is exercised.
5. **Provider support matrix.** Which non-MinIO target is mandatory for launch:
   AWS S3, R2, or both?

## Revision log (round 3)

- Fixed #1: control is S3-capable in the same production slice; it no longer rejects remote `--blob-store` after VFS removal.
- Fixed #2: legacy control `BundleStore` VFS is deleted and purge semantics become `BlobStore::delete_app_manifests`.
- Fixed #3: `S3ObjectMeta.last_modified` is mandatory for successful GET/HEAD metadata.
- Fixed #4: LocalFs metadata sidecars move outside the data tree under `<root>/metadata`, with list walking only `<root>/objects`.
- Fixed #5: LocalFs sidecar commit order, rollback, and stale-sidecar handling are specified without claiming cross-file atomicity.
- Fixed #6: `Backend::get` takes a cap so every backend enforces decoded response limits before allocation and before callback base64 encoding.
- Fixed #7: S3 download-to-cache uses an already-open temp file from `DiskBlobCache::reserve_temp`, not a raw path.
- Fixed #8: cache publish conflict semantics define singleflight, no-clobber publish, existing-file verification, and byte accounting.
- Fixed #9: retry rules distinguish content-addressed conditional PUT from mutable `env.storage.put`, which is not retried after ambiguous send.
- Fixed #10: dirty-connection handling applies to every S3 operation and every error body, not only GET/LIST.
- Fixed #11: timeout mechanics are grounded in `compio::time::timeout` around `send()` plus per-chunk and total body timeouts.
- Fixed #12: added a CI dependency guard for `tokio` and banned S3 SDK/signing crates.
- Superseded #13: round 4 requires correcting any stale Cargo comment that claims `aws-sigv4` is acceptable.
- Fixed #14: LocalDisk dedup must verify existing bytes before returning `Deduped`, matching S3 integrity.
- Fixed #15: LocalFs prefix validation is deliberately changed through shared `validate_list_coords` and conformance tests.
- Superseded #16: round 4 moves `quick-xml`, `time`, and `httpdate` to `[workspace.dependencies]`.
- Fixed #17: `S3Client` is config-only; live `cyper::Client` values are request-scoped/current-thread and never moved across compio threads.
- Fixed #18: production `env.storage` wiring names the shared parser type and exact worker/cache field replacements.
- Fixed #19: `--check-config` now specifies exact parsed report keys and redaction behavior per binary.
- Fixed #20: SSE launch policy is chosen: default sends no SSE headers and explicit modes are tested.

## Revision log (round 4)

- Fixed #1: `LocalDiskBlobStore::get_blob_to_file` now copies into the supplied open file; hard-linking is only allowed through a separate path-publish API.
- Fixed #2: object and list validation now share one app/bucket/key grammar, and bucket validation is tightened for both paths.
- Fixed #3: `DiskBlobCache::reserve_temp` and `publish_temp` have explicit async signatures, temp ownership, close/sync, and return shape.
- Fixed #4: cache publish forbids overwrite-rename and requires hard-link or `open(create_new)` copy semantics.
- Fixed #5: S3 manifest purge now specifies paginated LIST, per-object retries, partial failure behavior, and idempotence before DB deletion.
- Fixed #6: `S3Client` is immutable `Send + Sync` config/credential state with no shared limiter in v1 and no stored live `cyper::Client`.
- Fixed #7: `env.storage` now has stable S3 error string classes for auth, too-large, invalid-response, retryable, conflict, and transport.
- Fixed #8: checksum behavior is explicit via `checksum=none|sha256` with provider defaults and AWS/R2/MinIO tests.
- Fixed #9: `dev_http=true` is loopback/localhost-only for local MinIO.
- Fixed #10: LocalFs sidecars now use object-first commit plus a file-generation marker, preventing failed object renames from publishing metadata.
- Fixed #11: `quick-xml`, `time`, and `httpdate` are added to `[workspace.dependencies]` and consumed with `workspace = true`.
- Fixed #12: credentials section states v1 supports one S3 identity per process for blob storage and `env.storage`.
- Fixed #13: parser now has `provider=aws|r2|minio|generic`; `region=auto` is accepted only for `provider=r2`.
- Fixed #14: gateway cache testing now includes two-process same-cache-root race coverage.
- Fixed #15: base64 cap math now defines padded STANDARD length, padding, and overflow handling before decode/encode.
- Fixed #16: manifest conditional PUT is called out as a deliberate semantic break with replay, divergent JSON, 409 ambiguity, and unsupported-provider behavior.

---

## v1 Streaming & Multipart — FULL SCOPE (supersedes every "deferred / buffered-only / out-of-scope" statement above)

Operator directive (2026-06-12): **full-featured S3 in v1 — streaming both
directions + multipart, defer nothing, no whole-object buffering caps.** The
sections above that say "V1 does not implement streaming SigV4", "`put_stream`
intentionally absent", "Multipart upload is out of scope", and the
`MAX_BLOB_BYTES`/`env.storage` whole-object caps are **superseded** by this
section. (Grounded in verified facts: `cyper-0.8.3` `Response::bytes_stream()`
exists for streaming downloads; each multipart `UploadPart` is an ordinary
buffered `Bytes` PUT, so it never needs cyper's `Send`-bound streaming *request*
body; the runtime already has both V8↔Rust stream bridges.)

### Why multipart (not SigV4 chunked) for streaming uploads
cyper's streaming request `Body` is `Send`-bound and the compio context is
`!Send`, so a true single-request streaming PUT is not viable. **Multipart is the
upload-streaming mechanism**: read the source in bounded `PART_SIZE` chunks (8
MiB default), each `UploadPart` is a normal buffered PUT of ≤ `PART_SIZE` →
**bounded memory, unbounded total size, no whole-object buffer**. Objects below
one part use a single `PutObject` (no multipart overhead).

### `compio-s3` client — multipart + streaming surface (added to §1)
```
async fn get_stream(&self, key) -> Result<(ObjectMeta, impl Stream<Item=Result<Bytes>>)>  // cyper bytes_stream()
async fn create_multipart(&self, key, content_type) -> Result<UploadId>
async fn upload_part(&self, key, upload_id, part_number, body: Bytes) -> Result<PartETag>
async fn complete_multipart(&self, key, upload_id, parts: &[PartETag]) -> Result<()>      // XML body
async fn abort_multipart(&self, key, upload_id) -> Result<()>
```
- Every part PUT + Create/Complete/Abort is SigV4-signed (hand-rolled). Part
  bodies use the part's SHA-256 in `x-amz-content-sha256` (signed payload — parts
  are bounded, so hashing is cheap).
- **Mandatory abort:** any error mid-upload ⇒ `abort_multipart` (orphaned parts
  are billed). Wrap the part loop in a guard that aborts on `Err`/drop.
- `complete_multipart` body is the `CompleteMultipartUpload` XML (part numbers +
  ETags); response is XML (parse with `quick-xml`, check for an in-200-body error).

### `S3BlobStore::put_blob_stream` — streaming + content-addressed integrity
`put_blob_stream` already hands a single-pass `&mut dyn Read` + the expected
SHA-256 hash. Stream it:
1. Read in `PART_SIZE` chunks, feeding **two** sinks per chunk: a running
   SHA-256 hasher (whole-object) and the current part buffer.
2. When a part buffer fills → `upload_part`. Below `PART_SIZE` total → single
   `put` instead (skip multipart).
3. After the reader is exhausted: **verify `sha256(whole stream) == hash`** (the
   content address). On mismatch → `abort_multipart`, return `IntegrityError`,
   nothing is `Complete`d. Only on match → `complete_multipart`.
   (S3's own multipart ETag is not a usable content hash; client-side full-stream
   hashing is the integrity source of truth — same guarantee `LocalDiskBlobStore`
   gives, preserved across parts.)
4. `get_blob` (≤ a few MiB blobs in practice, but no cap): stream via `get_stream`
   into a buffer/temp-file while re-hashing; verify before returning.
- Idempotent dedup: `head` first; present ⇒ drain reader + `Deduped`.

### `env.storage` streaming through V8 — reuse existing bridges (supersedes the buffered §)
The `Backend` trait gains streaming methods (breaking it is allowed pre-launch;
update `LocalFs` + `S3` + every caller/fixture in the same patch):
```
async fn put_stream(&self, coords, body: impl Stream<Item=Result<Bytes>>, content_type) -> Result<()>
async fn get_stream(&self, coords) -> Result<Option<(ObjectMeta, impl Stream<Item=Result<Bytes>>)>>
```
- **Upload (`env.storage.put`)** consumes a V8 `ReadableStream` via the EXISTING
  `crates/runtime/src/web/streams/response_forwarder.rs` machinery (it already
  drives `getReader()` + a promise-reaction read loop into a Rust channel — the
  same path the RPC response-body forwarder uses). The native callback feeds
  those chunks to `Backend::put_stream` → S3 multipart. `LocalFs::put_stream`
  writes chunks to a temp file + atomic rename.
- **Download (`env.storage.get`)** returns a V8 `ReadableStream` fed by
  `crates/runtime/src/core/channel.rs` `StreamWriter`/`stream_buffer` (the exact
  bridge the RPC/SSE streaming path uses): a compio task pulls
  `Backend::get_stream` chunks and `push`es them to the `StreamWriter`; the
  worker exposes the `StreamReader` as the response body.
- The `@zeroship/storage` SDK grows streaming `put(key, ReadableStream|Blob)` /
  `get(key) -> { body: ReadableStream }` alongside the buffered convenience
  forms. Buffered `put(bytes)`/`get(): bytes` stay for small objects (built on
  the streaming path under the hood).
- **No whole-object caps.** Memory is bounded by `PART_SIZE` (upload) and the
  `StreamWriter`'s existing backpressure cap (download). The base64 buffered
  convenience path keeps a sane per-call cap; the streaming path has none.

### Build sequence (revised for full scope)
1. `crates/compio-s3`: client + hand-rolled SigV4 + **multipart** + `get_stream`;
   unit tests (SigV4 vectors) + MinIO smoke incl. a multipart round-trip.
2. `S3BlobStore` (streaming multipart put + content-address verify) + gateway
   refill (streaming) + `--blob-store` URL parse; control writes via the store.
3. `Backend` streaming methods + `plugin-storage::S3` (multipart) + `LocalFs`
   streaming + native callbacks wired to `response_forwarder`/`StreamWriter` +
   `@zeroship/storage` SDK streaming.
4. `tests/e2e_s3_storage.sh` (MinIO): deploy + dispatch over S3 blobs, and a
   **large multipart `env.storage` streaming** round-trip (put a >part-size
   ReadableStream, get it back as a stream, byte-compare). Backend-parity tests
   (LocalFs vs S3) for both buffered and streaming. Docs.

### Remaining operator open questions (NO feature deferrals)
1. `PART_SIZE` default (8 MiB proposed; S3 min part is 5 MiB except the last).
2. Provider matrix for launch (S3 + R2 both; MinIO always for tests).
3. Gateway disk-cache defaults (keep 20 GiB / 256 MiB).
