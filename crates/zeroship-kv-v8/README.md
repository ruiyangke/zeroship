# zeroship-kv-v8

The V8 binding for [zeroship-kv](../zeroship-kv/README.md). `KvBinding` implements
the runtime's `NativePlugin` interface and installs `env.kv` on each isolate.

The host opens a `zeroship_kv::KvStore` from runtime `KvConfig`, then passes it
and the optional process meter to `KvBinding::new`. The runtime supplies the
trusted app identity when it builds the instance; the binding issues a scoped
Rust `Kv` handle using `Namespace::app`. Backend selection happens at host
startup. Cargo features determine which implementations the host can configure.

- `lib.rs`: runtime registration and per-app meter injection.
- `v8_class.rs`: JavaScript argument extraction, synchronous validation, and
  instance lifetime.
- `limits.rs`: JavaScript numeric option conversion.
- `error.rs`: storage errors mapped to TypeErrors or coded promise rejections.
- `dispatch.rs`: scheduling scoped Rust operations, result conversion, and
  successful-op metering.
- `tests/e2e_runtime.rs`: the real V8-to-storage path, including metering.

The public Rust surface is `KvBinding`. Storage types are imported directly from
`zeroship-kv`; this crate does not re-export them. Its tests enable both backend
implementations through a development dependency. Run them with
`cargo test -p zeroship-kv-v8` and an available Docker daemon: Testcontainers
owns the Redis and Dragonfly cluster fixtures, using the shared test support
in `zeroship-kv/tests/support/mod.rs`. No backend environment variables or
manual provisioning are required.
