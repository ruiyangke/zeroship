# Native Plugin System

The runtime-native plugin interface is defined in [crates/runtime/src/core/plugin.rs](../../crates/runtime/src/core/plugin.rs). Built-in creator-facing namespaces currently come from [crates/plugin-db/src/lib.rs](../../crates/plugin-db/src/lib.rs), [crates/plugin-kv/src/lib.rs](../../crates/plugin-kv/src/lib.rs), and [crates/plugin-storage/src/lib.rs](../../crates/plugin-storage/src/lib.rs).

## Core trait

`NativePlugin` currently exposes:

- `namespace()`
- `name()`
- `register(&mut NativeRegistrar)`
- optional `build_instance(...)`

`NativeRegistrar` exposes:

- `add(...)`
- `add_setup(...)`

The runtime builds `env` by merging user env vars and secrets with plugin namespaces, then shallow-freezes the resulting object. That behavior is implemented in [crates/runtime/src/core/plugin.rs](../../crates/runtime/src/core/plugin.rs).

## Current plugin styles

There are two active patterns in the tree:

- Instance-backed namespaces: `plugin-db` and `plugin-kv` create V8 class instances through `build_instance(...)`.
- Flat callback namespaces: `plugin-storage` registers functions onto `env.storage`.

See [crates/plugin-db/src/lib.rs](../../crates/plugin-db/src/lib.rs), [crates/plugin-kv/src/lib.rs](../../crates/plugin-kv/src/lib.rs), and [crates/plugin-storage/src/lib.rs](../../crates/plugin-storage/src/lib.rs).

## `env.storage`: pluggable backend + streaming surface

`plugin-storage` is a flat-callback namespace whose object-store CRUD is
dispatched through a `Backend` trait. Two backends ship:

- **`LocalFs`** — the dev default. Objects live under
  `<root>/objects/<app_id>/<bucket>/<key>`; content-type/size/sha256 sidecars
  live under `<root>/metadata/...` so they never appear in `list`.
- **`S3`** (feature `s3`) — S3/R2/MinIO/Spaces/B2 over the bespoke
  `compio-s3` client (hand-rolled SigV4, cyper transport, **zero tokio**).

The backend is selected by a single URL grammar — the same one the deploy
blob store uses (`crates/plugin-storage/src/config.rs`,
`StorageBackendConfig::parse`):

```text
/var/lib/zeroship/storage            # LocalFs (bare path)
file:///var/lib/zeroship/storage     # LocalFs
s3://bucket/storage?region=us-east-1                                  # AWS
s3://bucket/storage?provider=r2&endpoint=https://<acct>.r2.cloudflarestorage.com&region=auto&style=path
s3://bucket/storage?provider=minio&endpoint=http://127.0.0.1:9000&region=us-east-1&style=path&dev_http=true
```

Wired on the worker as `--storage-url` / `ZEROSHIP_STORAGE_URL`, and on
`zeroship serve` as `ZEROSHIP_STORAGE_URL` (default
`file://.zeroship/storage`). S3 credentials resolve from the AWS env vars
(one S3 identity per process, shared with `--blob-store`).

### Native surface (what `@zeroship/storage` wraps)

Buffered (small objects, base64 over the JSON wire):

- `env.storage.put(bucket, key, bytesBase64, contentType?)`
- `env.storage.get(bucket, key)`
- `env.storage.delete(bucket, key)`
- `env.storage.list(bucket, prefix)`

Streaming (no whole-object buffering — bounded memory):

- `env.storage.putStream(bucket, key, ReadableStream, contentType?)`
- `env.storage.getStream(bucket, key)` → `{ streamId, contentType, size }`
- `env.storage.readChunk(streamId)` / `env.storage.cancelStream(streamId)`

`put_stream`/`get_stream` on the `Backend` trait are the kernel ops; buffered
`put`/`get` are conveniences built on the streaming path. The **upload** side
consumes a V8 `ReadableStream` through the existing
`crates/runtime/src/web/streams/response_forwarder.rs` read loop; the
**download** side feeds a V8 `ReadableStream` from the
`crates/runtime/src/core/channel.rs` `StreamWriter` bridge (the same machinery
the RPC/SSE streaming path uses). On the `S3` backend `put_stream` becomes an
**S3 multipart upload** (8 MiB parts; a sub-part object is a single
`PutObject`), so a creator can stream an unbounded object up and back with
memory bounded by the part size. Content-addressing integrity for deploy
blobs is preserved by hashing the whole stream client-side and verifying
before `complete_multipart`.

## Boundary

Creator-facing APIs should stay small. If a feature can be expressed in JS on top of `fetch` or the existing native primitives, it belongs in an SDK package rather than a new runtime plugin.

Platform-only DB internals are not part of the public plugin contract. The bootstrap layer resolves those privately when installing schema; creator code should treat `env.db` as the typed document API described in [docs/reference/db.md](../reference/db.md).
