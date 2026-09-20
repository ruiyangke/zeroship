# zeroship-kv

Key-value storage for Rust platform services and creator apps. `KvStore` opens
the backend selected by host runtime configuration. It issues cloneable `Kv`
handles bound to a validated app or platform `Namespace`. Operations validate
input and return `KvError` and `TtlState` without a V8 or metering dependency.

The `redb` feature enables embedded persistence and file-lock diagnostics. The
`redis` feature enables the Redis-compatible backend, including standalone,
cluster, and Sentinel routing.
Standalone defaults enable both; workspace hosts enable the implementations
their configuration permits. `KvConfig` selects the active implementation at
startup. Selecting an implementation absent from the binary is an error, not a
fallback. Disable default features to supply a custom `Backend` through
`KvStore::from_backend`.

Hosts translate their settings into `KvConfig::Redis { redis }` or
`KvConfig::Redb { path }`; this crate does not read environment variables.
The same configuration can be parsed with `KvConfig::from_toml`. See the
[configuration reference](../../docs/architecture/kv-configuration.md) for host
settings, topology examples, authentication, TLS, and recovery.
Open the store at startup and inject scoped handles into application state:

```rust
use zeroship_kv::{Kv, KvConfig, KvError, KvStore, Namespace};

fn platform_kv(config: &KvConfig) -> Result<Kv, KvError> {
    let store = KvStore::open(config)?;
    Ok(store.namespace(Namespace::platform("control")?))
}

async fn refresh(kv: &Kv) -> Result<(), KvError> {
    kv.set("refresh-status", "ready", None).await
}
```

Store and handle clones share the backend. Embedded storage opens its file at
startup, creating missing parent directories. Redis connects lazily on each
compio thread. Rust values are strings; callers own serialization. Direct Rust
calls do not emit creator usage metrics.

`Namespace::app` accepts a trusted app identity; `Namespace::platform` reserves
a separate keyspace for an internal subsystem. A namespace separates keys,
not credentials: platform-private stores must use credentials unavailable to
creator workers. Give application code a `Kv`, keeping store ownership and
namespace selection at the host boundary.

`Backend` is a low-level interface for trusted Rust hosts. Its `app_id` argument
must come from the host's tenant identity, never from caller-controlled options.
Ordinary callers use `Kv`, which enforces shared key/value and TTL validation.
Direct backend implementors and trusted hosts can use `limits` themselves.
Futures are thread-local to support compio.

`backend/mod.rs` defines the operation and atomicity contracts.
`config.rs`, `store.rs`, and `namespace.rs` define runtime selection and the
scoped Rust interface.
`backend/redb.rs` owns persistent embedded storage; `backend/redis.rs` owns
Redis command selection and connection caching. `error.rs` classifies failures
without deciding how a language binding presents them.

The V8 adapter lives in [zeroship-kv-v8](../zeroship-kv-v8/README.md).
It supplies JavaScript argument conversion, per-isolate scope, promise handling,
and platform usage metering over the same scoped Rust operations.

Run the embedded suite with:

```sh
cargo test -p zeroship-kv --no-default-features --features redb
```

`cargo test -p zeroship-kv` also runs the live Redis and Dragonfly deployment suites.
Testcontainers starts isolated databases, configures cluster slots, and removes
containers when their tests finish. Docker must be available; startup failures
fail the run. No backend URLs, Compose setup, or bootstrap scripts are needed.
Shared cluster fixtures live in `libs/compio-redis/tests/common/containers.rs`
and are reused by the KV and V8 binding tests. `tests/topologies.rs` exercises deployment discovery,
Sentinel failover, and authenticated TLS. Testcontainers is a development-only dependency; its
Docker orchestration runs independently of the compio database operations.

`tests/architecture.rs` checks the dependency boundary with every storage
feature enabled. `tests/state_dir_lock_marker.rs` checks the Vite diagnostic
token against the compiled Rust constant. Both run through ordinary Cargo tests.

The driver, store, and binding suites also run with nextest:

```sh
cargo nextest run -p compio-redis -p zeroship-kv -p zeroship-kv-v8 --test-threads 2
```

The dashboard owns its public SDK and browser acceptance tests:

```sh
pnpm install
pnpm build
pnpm --dir examples/kv-dashboard test
```

The example's TypeScript setup provisions its platform with Testcontainers, then
Vitest checks RPC behavior and Playwright checks the UI against redb and Redis.
Use `pnpm --dir examples/kv-dashboard smoke` for an already-running dashboard.
See the [example README](../../examples/kv-dashboard/README.md) for browser setup,
URL selection and failure artifacts.
