# zeroship-storage

Object storage for Rust callers, independent of V8 and metering. The host owns a
`StorageStore` and issues `Storage` handles with a fixed `Namespace`. Applications
can choose bucket names and object keys within their namespace.

```rust
use zeroship_storage::{Namespace, StorageBackendConfig, StorageStore};

async fn save_report() -> Result<(), Box<dyn std::error::Error>> {
    let config = StorageBackendConfig::parse("file://.zeroship/storage")?;
    let store = StorageStore::open(&config)?;
    let storage = store.namespace(Namespace::platform("control")?);
    storage.put("reports", "latest.txt", b"ready", Some("text/plain")).await?;
    Ok(())
}
```

Run operations inside a compio runtime. `StorageStore::from_backend` accepts a
host-owned backend, and `with_limits` supplies limits from code. Streaming
sources implement `backend::ChunkSource`; dropping a download releases it.

`LocalFs` stores objects and metadata in separate subtrees. The `s3` feature,
enabled by default, supplies S3-compatible storage through `compio-s3`; runtime
configuration chooses the backend. Neither backend depends on the V8 binding.

App and platform namespaces are disjoint. Namespace constructors validate names;
the trusted host authenticates the identity it supplies. Sensitive platform
storage needs credentials unavailable to creator workers.

`cargo test -p zeroship-storage` runs scoped storage, backend parity and dependency
boundary tests. S3 tests own their MinIO containers through Testcontainers and
require Docker. `--no-default-features` also checks the local-only build.
