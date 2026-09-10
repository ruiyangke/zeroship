# zeroship-kv

App-scoped key-value storage with an async Rust interface. This crate owns the
`Backend` contract, `KvError`, `TtlState`, key scoping, shared limits, and storage
implementations. It has no V8, runtime, or metering dependency.

The `redb` feature enables embedded persistence and file-lock diagnostics. The
`redis` feature enables the compio Redis driver, including cluster routing.
Standalone defaults enable both; workspace consumers select their backends
explicitly. Disable default features to implement the trait without either
built-in backend.

`Backend` is a low-level interface for trusted Rust hosts. Its `app_id` argument
must come from the host's tenant identity, never from caller-controlled options.
Hosts validate creator input before calling it: `limits` provides key/value
validation and the creator-facing limit constants. Values are strings; JSON
encoding belongs to the caller. Futures are thread-local to support compio.

`backend/mod.rs` defines the operation and atomicity contracts.
`backend/redb.rs` owns persistent embedded storage; `backend/redis.rs` owns
Redis command selection and connection caching. `error.rs` classifies failures
without deciding how a language binding presents them.

The V8 adapter lives in [zeroship-kv-v8](../zeroship-kv-v8/README.md).
It supplies JavaScript argument conversion, per-isolate scope, promise handling,
and platform usage metering. Direct Rust calls are not metered by this crate.

Run the embedded suite with:

```sh
cargo test -p zeroship-kv --no-default-features --features redb
```

`cargo test -p zeroship-kv` also runs the live Redis and Dragonfly cluster suite.
Its fixture requirements are documented in `tests/redis_backend.rs`; missing
backends fail the run. `tests/kv_crate_closure_gate.sh` at the workspace root
checks the dependency boundary with every storage feature enabled.
