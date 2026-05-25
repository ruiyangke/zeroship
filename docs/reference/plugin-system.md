# Native Plugin System

The runtime-native plugin interface is defined in [crates/runtime/src/core/plugin.rs](crates/runtime/src/core/plugin.rs). Built-in creator-facing namespaces currently come from [crates/plugin-db/src/lib.rs](crates/plugin-db/src/lib.rs), [crates/plugin-kv/src/lib.rs](crates/plugin-kv/src/lib.rs), and [crates/plugin-storage/src/lib.rs](crates/plugin-storage/src/lib.rs).

## Core trait

`NativePlugin` currently exposes:

- `namespace()`
- `name()`
- `register(&mut NativeRegistrar)`
- optional `build_instance(...)`

`NativeRegistrar` exposes:

- `add(...)`
- `add_setup(...)`

The runtime builds `env` by merging user env vars and secrets with plugin namespaces, then shallow-freezes the resulting object. That behavior is implemented in [crates/runtime/src/core/plugin.rs](crates/runtime/src/core/plugin.rs).

## Current plugin styles

There are two active patterns in the tree:

- Instance-backed namespaces: `plugin-db` and `plugin-kv` create V8 class instances through `build_instance(...)`.
- Flat callback namespaces: `plugin-storage` registers functions onto `env.storage`.

See [crates/plugin-db/src/lib.rs](crates/plugin-db/src/lib.rs), [crates/plugin-kv/src/lib.rs](crates/plugin-kv/src/lib.rs), and [crates/plugin-storage/src/lib.rs](crates/plugin-storage/src/lib.rs).

## Boundary

Creator-facing APIs should stay small. If a feature can be expressed in JS on top of `fetch` or the existing native primitives, it belongs in an SDK package rather than a new runtime plugin.

Platform-only DB internals are not part of the public plugin contract. The bootstrap layer resolves those privately when installing schema; creator code should treat `env.db` as the typed document API described in [docs/reference/db.md](docs/reference/db.md).
