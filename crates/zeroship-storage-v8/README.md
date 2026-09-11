# zeroship-storage-v8

Binds `zeroship-storage` to `env.storage` through `StorageBinding::new(store,
meter)`. The host selects the store; the binding captures an app-scoped Rust
handle when the runtime initializes the namespace.

This crate owns JavaScript argument conversion, promises, upload backpressure,
download handles and platform usage metering. Rust storage consumers depend on
`zeroship-storage` directly.

Download sources belong to their isolate. Stream handles cannot cross apps or
deploys, and isolate teardown releases undrained downloads. Quota counters are
shared across isolates of the same app on a worker thread, so creating another
isolate cannot multiply its download budget.

`cargo test -p zeroship-storage-v8` exercises real V8 streaming, metering,
namespace isolation, backend binding and resource reclamation.
