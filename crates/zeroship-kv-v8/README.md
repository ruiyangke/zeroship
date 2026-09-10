# zeroship-kv-v8

The V8 binding for [zeroship-kv](../zeroship-kv/README.md). `KvBinding` implements
the runtime's `NativePlugin` interface and installs `env.kv` on each isolate.

The host selects and constructs a `zeroship_kv::Backend`, then passes its shared
handle and process meter to `KvBinding::with_backend_and_meter`. The runtime
supplies the app scope when it builds the instance. Backend selection and
storage features belong to the host's `zeroship-kv` dependency.

- `lib.rs`: runtime registration and per-app meter injection.
- `v8_class.rs`: JavaScript argument extraction, synchronous validation, and
  instance lifetime.
- `limits.rs`: JavaScript numeric option conversion.
- `error.rs`: storage errors mapped to TypeErrors or coded promise rejections.
- `dispatch.rs`: async scheduling, result conversion, and successful-op metering.
- `tests/e2e_runtime.rs`: the real V8-to-storage path, including metering.

The public Rust surface is `KvBinding`. Storage types are imported directly from
`zeroship-kv`; this crate does not re-export them. Its tests enable both backend
implementations through a development dependency. Run them with
`cargo test -p zeroship-kv-v8` and an available Docker daemon: Testcontainers
owns the Redis and Dragonfly cluster fixtures, using the shared test support
in `zeroship-kv/tests/support/mod.rs`. No backend environment variables or
manual provisioning are required.
