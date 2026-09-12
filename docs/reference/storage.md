# Object storage

`zeroship-storage` provides scoped Rust object storage. `zeroship-storage-v8`
binds it to `env.storage`, which the `@zeroship/storage` SDK wraps for app code.
The Rust crate has no dependency on V8, the worker or metering.

```text
Rust platform service                     Creator app
        |                                      |
        |                              @zeroship/storage
        |                                      |
        |                          zeroship-storage-v8
        |                           binding + metering
        |                                      |
        +------------ zeroship-storage --------+
                      StorageStore
                           |
                  Storage(namespace)
                           |
                      Backend trait
                       /         \
                   LocalFs        S3
                                  |
                              compio-s3
```

## Rust use

The host opens a store once and gives each caller a bound `Storage` handle.
Its operations accept bucket names and keys; they cannot change the namespace.
Run the async operations inside a compio runtime.

```rust
use zeroship_storage::{Namespace, StorageBackendConfig, StorageStore};

async fn save_report() -> Result<(), Box<dyn std::error::Error>> {
    let config = StorageBackendConfig::parse("file://.zeroship/storage")?;
    let store = StorageStore::open(&config)?;
    let storage = store.namespace(Namespace::platform("control")?);

    storage.put("reports", "latest.txt", b"ready", Some("text/plain")).await?;
    let report = storage.get("reports", "latest.txt").await?;
    assert!(report.is_some());
    Ok(())
}
```

For an app, use `Namespace::app(authenticated_app_id)`. For backend injection,
use `StorageStore::from_backend(Arc<dyn Backend>)`; `with_limits(StorageLimits)`
sets limits in code. An explicitly constructed `S3` can receive credentials and
`S3UploadTuning` through `S3::with_tuning`. Injection lets Rust tests and platform
services configure storage without changing process environment variables.

`Storage` exposes `put`, `get`, `put_stream`, `get_stream`, `delete` and `list`.
Streaming bodies implement `backend::ChunkSource`. A Rust download belongs to
its caller and closes when dropped. Errors distinguish invalid arguments, size
limits, backend failures and body-stream failures through `StorageError`.

## Configuration

Backend selection happens at runtime. The `s3` Cargo feature enables the S3
implementation; it is enabled by default in the Rust crate and explicitly by
the worker and CLI. A local-only Rust consumer can disable default features.

| Location | Backend |
| --- | --- |
| `/var/lib/zeroship/storage` | Local filesystem |
| `file://.zeroship/storage` | Local filesystem |
| `s3://app-objects/storage?region=us-east-1` | AWS S3 |
| `s3://app-objects/storage?provider=r2&endpoint=https://ACCOUNT.r2.cloudflarestorage.com&region=auto&style=path` | Cloudflare R2 |
| `s3://app-objects/storage?provider=minio&endpoint=http://127.0.0.1:9000&region=us-east-1&style=path&dev_http=true` | Local MinIO |

The worker accepts `--storage-url`, `ZEROSHIP_WORKER_STORAGE_URL`, or its TOML
setting:

```toml
[worker]
storage_url = "s3://app-objects/storage?region=us-east-1"
```

An empty worker setting leaves `env.storage` unavailable. `zeroship serve` uses
`ZEROSHIP_STORAGE_URL` and defaults to `file://.zeroship/storage`.
`StorageBackendConfig::parse` owns the shared location grammar; empty locations
and unsupported URL schemes fail validation.

`StorageStore::open` resolves S3 credentials from `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY` and optional `AWS_SESSION_TOKEN`. The credentials must
be able to access the configured physical S3 bucket. The logical bucket names
used by apps are key prefixes within that bucket.

| Environment setting | Controls |
| --- | --- |
| `ZEROSHIP_STORAGE_MAX_OBJECT_BYTES` | Buffered reads and writes |
| `ZEROSHIP_STORAGE_MAX_STREAM_BYTES` | Total bytes in a streamed upload |
| `ZEROSHIP_STORAGE_UPLOAD_CONCURRENCY` | Concurrent S3 multipart part uploads |
| `ZEROSHIP_STORAGE_MAX_LIVE_GET_STREAMS` | Open V8 downloads per app on a worker thread |

Defaults and clamping live in the Rust crate's `limits` module and the V8
binding's `limits` module. Object limits apply through scoped handles for both
backends. These are operation limits; aggregate stored-byte quotas are not
implemented by this layer.

## App isolation

The trusted host supplies the app identity when constructing the isolate.
The V8 binding captures the store, validated app namespace and meter then;
callbacks never read an app-selected namespace. Missing or invalid host identity
fails runtime initialization. The scoped Rust API validates buckets, object
keys, list prefixes and cursors before dispatching to a backend.

```text
LocalFs: <root>/<namespace>/<logical-bucket>/o/<key>  object
                                             m/<key>  metadata

S3:      <configured-prefix>/<namespace>/<logical-bucket>/<key>
```

`Namespace::platform("control")` uses a reserved delimiter that
`Namespace::app` rejects, keeping platform and app namespaces disjoint.
Namespace construction validates names; it does not authenticate the host.
Rust code that owns the unscoped store or raw backend is trusted. Private
platform data requiring protection from the worker process needs separate
storage credentials unavailable to that process.

Download handles belong to their isolate. Another app, or another isolate of
the same app, cannot read or cancel those sources. Isolate teardown releases
undrained downloads. The live-download quota is shared across an app's isolates
on a worker thread.

## JavaScript binding and verification

`StorageBinding::new(store, meter)` registers the native buffered operations
`put`, `get`, `delete` and paginated `list`, plus `putStream`, `getStream`,
`readChunk` and `cancelStream`. The SDK converts these into typed bucket
operations and `ReadableStream` downloads. Metering stays in the binding.

`LocalFs` stores content-type sidecars outside the listed object subtree and
runs its directory walk on the blocking pool. S3 uploads use bounded multipart
buffers and abort on producer failure. Backend parity tests exercise these
behaviors against a Testcontainers-owned MinIO.

```sh
cargo test -p zeroship-storage -p zeroship-storage-v8 -p compio-s3 --all-features
cargo test -p zeroship-storage --no-default-features
pnpm --filter @zeroship/storage test
```

S3 tests require Docker and provision their own containers; an unavailable
container runtime fails verification. The storage examples own their deployment
tests in Vitest and browser checks in Playwright. `cargo xtask test storage` runs the Rust suites and both example
suites. Their Testcontainers fixtures exercise real control, gateway and worker
processes, including S3-backed deploy artifacts and a stream crossing the `u32`
length boundary.
