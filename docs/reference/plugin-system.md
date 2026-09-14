# Native Plugin System

The runtime-native plugin interface is defined in [crates/zeroship-runtime/src/core/plugin.rs](../../crates/zeroship-runtime/src/core/plugin.rs). Built-in creator-facing namespaces currently come from [crates/zeroship-data-v8/src/lib.rs](../../crates/zeroship-data-v8/src/lib.rs), [crates/zeroship-kv-v8/src/lib.rs](../../crates/zeroship-kv-v8/src/lib.rs), and [crates/zeroship-storage-v8/src/lib.rs](../../crates/zeroship-storage-v8/src/lib.rs).

## Core trait

`NativePlugin` currently exposes:

- `namespace()`
- `name()`
- `register(&mut NativeRegistrar)`
- optional `build_instance(...)`
- optional `bind_runtime_descriptor(...)`
- optional `javascript_modules()`
- optional `prepare_runtime(...)`
- optional `finalize_runtime(...)`

`NativeRegistrar` exposes:

- `add(...)`
- `add_setup(...)`

The runtime builds `env` by merging user env vars and secrets with plugin namespaces, then shallow-freezes the resulting object. That behavior is implemented in [crates/zeroship-runtime/src/core/plugin.rs](../../crates/zeroship-runtime/src/core/plugin.rs).

## JavaScript adapters

A plugin can return compiled SDK sources as `JavaScriptModule` entries from
`javascript_modules()`. Each specifier belongs to its
`zeroship:<namespace>/` prefix. The runtime compiles their dependency graph and
shares module instances across static and dynamic imports. Creator artifacts
cannot replace these sources. An adapter may import other registered adapters,
native modules and `zeroship`; it cannot statically import creator modules.

Module delivery grants no additional authority. Privileged finalization remains
in native lifecycle hooks; adapter JavaScript uses the app-scoped primitives.

The [DB adapter](../../crates/zeroship-data-v8/src/lib.rs) supplies the DB SDK
internal entry as `zeroship:db/internal`. Its source and build dependency belong
to `zeroship-data-v8`; the runtime core loads the registered module graph.

## Startup lifecycle

The runtime binds the validated descriptor and compiles the module graph before
calling `prepare_runtime`. A plugin can use `modules::invoke_module_export` to
prepare its SDK facade. Startup retains the returned promise and drives native
operations until preparation settles, before evaluating creator modules.

After creator evaluation settles, the runtime calls the synchronous
`finalize_runtime` hooks before publishing dispatch handlers. Startup runs
without a request identity. Its failure is cached; queued requests receive the
same startup diagnostic. Wall and CPU limits apply while startup is pending,
and the CPU budget carries across asynchronous continuations.

`Runtime::initialize(...).await` completes when the runtime is ready or fails.
The worker awaits it before caching an isolate. Hosts that manage multiple
isolates keep them exited between asynchronous turns; initialization enters
and exits the isolate around its synchronous V8 work.

DB facade preparation uses this lifecycle. The SDK records mask-policy
declarations through the native DB binding while creator startup evaluates.
The DB plugin installs and seals the captured declaration during finalization.
Unmasking refuses with `database_startup_pending` until finalization; ordinary
descriptor-bound operations remain available during startup.

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

## Continuation state

Native callbacks read the invoking procedure's kind with
`zeroship_runtime::rpc::current_kind(scope)` before queueing work. The frame
follows V8 continuations across `await`; a thread-local marker cannot distinguish
overlapping requests or isolates. Native callers use `with_kind(scope, kind,
call)` to enter a procedure while restoring the caller's context when the
synchronous V8 call returns.

`rpc::capability::current_procedure_frame(scope)` provides opaque storage for
plugin-owned state with that frame's lifetime. Attach state through an
isolate-private V8 key. The DB adapter uses it to retain the query's read
capture, enters the capture during preparation and each poll of queued native
work, and snapshots it when opening a subscription. A suspended operation
keeps its capture without leaving it active on the host thread.

Implementations: [procedure frames](../../crates/zeroship-runtime/src/rpc/capability.rs),
[DB capture ownership](../../crates/zeroship-data-v8/src/read_capture.rs), and
[scoped ORM capture](../../crates/zeroship-data-orm/src/cdc/read_set/capture.rs).

## Boundary

Creator-facing APIs should stay small. If a feature can be expressed in JS on top of `fetch` or the existing native primitives, it belongs in an SDK package rather than a new runtime plugin.

Platform-only DB internals are not part of the public plugin contract. `DbPlugin`
registers its private adapter module and native startup invokes it with the
validated descriptor and DB handle. Creator code should treat `env.db` as the
typed document API described in [db.md](db.md).
