# Native Plugin System

The runtime-native plugin interface is defined in [crates/zeroship-runtime/src/core/plugin.rs](../../crates/zeroship-runtime/src/core/plugin.rs). Built-in creator-facing namespaces currently come from [crates/zeroship-data-v8/src/lib.rs](../../crates/zeroship-data-v8/src/lib.rs), [crates/zeroship-kv-v8/src/lib.rs](../../crates/zeroship-kv-v8/src/lib.rs), and [crates/zeroship-storage-v8/src/lib.rs](../../crates/zeroship-storage-v8/src/lib.rs).

## Core trait

`NativePlugin` currently exposes:

- `namespace()`
- `name()`
- `register(&mut NativeRegistrar)`
- optional `build_instance(...)`

`NativeRegistrar` exposes:

- `add(...)`
- `add_setup(...)`

The runtime builds `env` by merging user env vars and secrets with plugin namespaces, then shallow-freezes the resulting object. That behavior is implemented in [crates/zeroship-runtime/src/core/plugin.rs](../../crates/zeroship-runtime/src/core/plugin.rs).

## Current plugin styles

There are two active patterns in the tree:

- Instance-backed namespaces: the DB and KV bindings create V8 class instances through `build_instance(...)`.
- Flat callback namespaces: `storage-v8` registers functions onto `env.storage`.

See [crates/zeroship-data-v8/src/lib.rs](../../crates/zeroship-data-v8/src/lib.rs), [crates/zeroship-kv-v8/src/lib.rs](../../crates/zeroship-kv-v8/src/lib.rs), and [crates/zeroship-storage-v8/src/lib.rs](../../crates/zeroship-storage-v8/src/lib.rs).

## `env.storage`

`zeroship-storage-v8` registers flat callbacks and uses `build_instance` to
capture a scoped Rust storage handle in the isolate. The host supplies a
`StorageStore` through `StorageBinding::new(store, meter)`. Backend selection,
validation and object operations live in `zeroship-storage`; V8 conversion,
download handles and metering live in the binding.

See [Object storage](storage.md) for Rust usage, runtime configuration,
namespace isolation, streaming and verification.

## Boundary

Creator-facing APIs should stay small. If a feature can be expressed in JS on top of `fetch` or the existing native primitives, it belongs in an SDK package rather than a new runtime plugin.

Platform-only DB internals are not part of the public plugin contract. The bootstrap layer resolves those privately when installing schema; creator code should treat `env.db` as the typed document API described in [docs/reference/db.md](../reference/db.md).
